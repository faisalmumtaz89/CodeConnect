# CodeConnect — Mac side

Three shipped binaries and a shared library in one cargo workspace, plus a chaos
harness that is not shipped.

| Crate | What it is |
|---|---|
| `protocol` | Shared wire types: event envelope, unix-socket frames, WebSocket protocol, session identity, config. |
| `ccd` | The daemon: SQLite event log, hook gate, transcript tailer, tailnet WebSocket server, push. |
| `codeconnect` | The shim: `codeconnect claude` hosts a session in `tmux -L codeconnect` and attaches in place. Also the per-session supervisor and the LaunchAgent lifecycle. |
| `cc-hook` | The tiny binary Claude Code invokes on every wired hook event. |
| `soak` | Not shipped. The chaos gauntlet — see [Soaking it](#soaking-it). |

## Run it

```sh
./install.sh                      # build + install to ~/.codeconnect/bin
export PATH="$HOME/.codeconnect/bin:$PATH"

codeconnect daemon install                 # run ccd under launchd, restart it on crash
codeconnect claude                         # in any project directory
```

`codeconnect claude` passes every argument through to the real `claude`, so
`codeconnect claude --permission-mode default --resume` works exactly as expected.

```sh
codeconnect ls                    # sessions, identities and link state
codeconnect attach cc-1           # re-attach after closing the tab

codeconnect daemon install        # write the LaunchAgent and start it
codeconnect daemon status         # plist, launchd job and live daemon, side by side
codeconnect daemon restart        # launchctl kickstart -k
codeconnect daemon uninstall      # stop it and remove the LaunchAgent

codeconnect pair                  # QR code that pairs a phone (single use, 5 minutes)
codeconnect devices               # paired devices
codeconnect revoke <device>       # revoke a device's token
codeconnect token                 # the static fallback token
```

## Session identity

A session has two names and they do different jobs.

`session_id` is the tmux name — `cc-1` — and it is **reused**. `codeconnect claude` takes
the lowest free number, so when a session exits the next one is called `cc-1`
again. That is deliberate: the name is what a human types into `codeconnect attach`.

`session_uid` is a [ULID](https://github.com/ulid/spec) minted once at spawn and
never reused. It is what the event log, the tail cursors and the answers ledger
are keyed by.

The tmux name used to do both jobs, and a new `cc-1` inherited the dead one's
log and continued its `seq`. Two things were wrong with that:

* **History integrity** — one `cc-1` timeline on the phone was two unrelated
  agent runs spliced together, with no marker between them.
* **Approval safety** — a `request_id` answered in the first run stayed in the
  ledger under the same key, so a card from the second run that happened to
  reuse an id came back as an already-applied duplicate: an answer nobody gave.

Both identities ride every `event` and every entry in `sessions`. Anything that
names a session — `subscribe`, `answer`, `send_text`, `capture`, `get_diff` —
accepts either, so a client that only knows `cc-1` keeps working. A bare name
resolves to the run with a supervisor attached, and failing that to the newest
run under the name. `lifecycle` is deliberately not consulted: a session that
ended while the daemon was down was never *observed* exiting, so it reads `Live`
forever, and ranking on it would put that ghost above the run that is really
there. A client on `protocol_minor >= 2` should send the uid and key its own
store by it.

`codeconnect ls` shows both.

## Prompt identity

An approval card is bound to **one prompt**, and the daemon refuses to type an
answer it cannot prove is going to that prompt.

Three mechanisms, in decreasing order of certainty:

* **Generation.** Every structured `PermissionRequest` gives the run a new
  prompt generation — derived from the number of `approval_request` events it
  has logged, so a replayed hook cannot advance it and a restart cannot lose it.
  A card raised at generation *N* is refused once the run is on *N+1*, and the
  older card is **superseded**: it stops asking, and the ledger records
  `resolved_by: "superseded"` with nothing typed. Superseded is reported as a
  *rejection* rather than a duplicate, because a duplicate means "your answer
  already applied" and this one never did.
* **Visible pane only.** Every presence and identity check captures
  `capture-pane -p -J` with no `-S`, which is tmux's visible-pane default. With
  scrollback, "a permission prompt is on screen" stayed true for as long as one
  had *ever* been on screen — so a prompt answered half an hour ago could
  authorise typing into the composer.
* **Prompt fingerprint.** A sha256 of the prompt *block* on the visible pane:
  the line the presence needle matched, twelve lines of context above it (the
  command being asked about) and everything below it (the options and their
  footer). Not the whole pane — a pane carries a cursor and elapsed counters, so
  a whole-pane hash would refuse every answer. It rides with the injection
  request and is re-checked by the supervisor **in the same breath as the
  presence check**, immediately before the keys go out.

The fingerprint is taken a few hundred milliseconds after the card is created,
because the hook fires microseconds *before* Claude renders the prompt. Until it
is taken the card is shown but not remotely actuatable, and the binding is
announced as its own event (`approval_prompt_bound`, an `Other` kind so an older
client passes it through untouched). `ApprovalCard.identity_bound` on the
`approval_request` event is what was true when the card was logged — false, by
construction.

**Refusing is the answer when identity cannot be established.** No fingerprint,
a prompt that has changed, or a supervisor too old to check one, and the answer
is refused with a reason rather than typed on a guess. A free-text takeover is
exempt: it is not an answer to a prompt at all, and its interlock is the
composer being ready — which a permission prompt on screen already fails.

**A drawn composer is not a live one.** Since `protocol_minor` 9 the presence
check has a second half: the composer counts as ready only if the input-box
needle matches *and* the pane's cursor is visible (`#{cursor_flag}`, tmux's
record of the terminal's DECTCEM state). Measured: submitting `/status` while a
turn is running leaves Claude's Settings view drawn **above** the composer box
when the turn ends — the needle matches, typed text never appears, and Enter
does nothing. Without the second half the interlock authorises keys into a pane
that cannot receive them and reports them sent.

The signal is the cursor rather than the view's `Esc to cancel` hint because a
string is something an agent can write into its own output: a text rule would
refuse every send in a session that merely *discussed* the hint, and would
Escape a running turn to rescue it. Measured across states — idle, mid-turn
(327 consecutive samples), and after a turn: cursor visible; every view Claude
opens, including the one drawn above the composer: hidden. A cursor that cannot
be read is a refusal, not a permissive default.

**Word-shaped slash commands carry a postcondition.** After such a send the
supervisor looks at the pane at 1.5s and again at 3.0s (a large inline render —
measured on `/context` — can hide the footer for ~2.5s and restore it unaided),
and if the composer is gone both times it saves that frame, sends exactly one
`Escape`, and verifies for 250ms that the composer came back. It reports
`composer_recovered` — with the saved pane, for the three snapshot commands and
nothing else — or `composer_lost`, which asks for a human rather than guessing
further keys. One Escape is the ceiling on purpose: `/keybindings` spawns an
editor where Escape is a mode key, so no key sequence can rescue it, and typing
into an unknown screen is how a rescue becomes damage.

A supervisor from before `protocol_minor` 3 accepts the fingerprint field and
silently drops it (serde ignores what it does not know), so it would look
checked and be unchecked. Such a supervisor reports its level at registration
and the daemon refuses to actuate approvals through it: sessions started before
this upgrade can be *watched* from the phone but must be answered at the Mac
until they are restarted. It is logged once, at registration, rather than only
as a refusal somebody cannot explain.

## Durability of mutations

Anything that types into a TTY is claimed durably **before** it types, and the
claim is cleared in the same transaction as its outcome.

That two-phase claim exists for one question a restart could not otherwise
answer: *did the keystroke land?* A claim with no outcome means the daemon died
between the two, and recovery records an explicit indeterminate outcome —
`AnswerOutcome.indeterminate: true` for an approval, `SendTextResult` status
`indeterminate` for a takeover. It is **never** retried: a second injection into
a live TTY cannot be taken back, while an unanswered prompt is still sitting in
front of a human.

The same distinction applies without a crash. An injection has three outcomes,
not two: applied, *refused*, and *unconfirmed*. A refusal is a positive
statement that nothing was typed — the supervisor checks before it injects and
never after, and a request that never reached a supervisor at all obviously
typed nothing either — so the claim is released and the card can be answered
again. A request that *was* sent and never answered (a timeout, a supervisor
that vanished mid-flight) is different: it may have typed, and nothing later can
settle that, so it becomes an indeterminate outcome too. Collapsing those two
into one error is how a slow supervisor turns into a second keystroke — and
collapsing them the other way strands an answerable card behind a permanent "we
do not know".

Open approval cards are persisted too. They used to be memory-only, so a restart
answered "unknown or already-resolved request" to a tap on a card that was still
on the screen. A recovered card comes back with **no** fingerprint — this
process never saw the screen it was created against — and the local-resolution
sweep either re-establishes identity against the pane that is actually visible
or resolves the card because the prompt has gone.

## Never claiming what it does not know

Three places used to turn "we could not tell" into a confident answer. Each one
is now three-valued.

**A tmux error is not proof of death.** `has-session` exits non-zero for "there
is no such session" *and* for every way it can fail to look — a moved binary, an
unreachable socket, a process out of descriptors, a server mid-restart. Any
non-zero status was read as absence, and the supervisor turned absence straight
into a durable `SessionEnd`. Now tmux's own message is parsed: only a recognised
"no such session" wording counts as gone, anything else is *unknown* and logged,
and an exit is reported only after two consecutive confirmations (about four
seconds at the 2-second poll). A reported exit is a fact that cannot be
withdrawn, which is what makes the second look worth the wait.

**A session that starts and ends while `ccd` is down still lands.** It was never
registered, so the daemon had no row to attach the exit to, logged "exit
reported for unknown session" and dropped the frame: the run vanished entirely —
not in the fleet, no event log, nothing anywhere saying an agent had run. The
supervisor now replays its registration on the same connection, immediately
before the exit, so the daemon has something to record against. Idempotent by
construction: a session that *was* registered normally is simply re-registered
under the same uid, which is the path a supervisor reconnecting after a daemon
restart already takes.

**A transcript line too long to hold no longer stalls the tail.** A line with no
newline inside the 4MiB read window left nothing consumed, so the scan returned
early and the cursor never moved — and every later poll read the same bytes and
made the same decision. No transcript fact from that session was ever ingested
again, silently, because the code path is indistinguishable from an idle file.
The window is now consumed with an explicit `error{transcript_line_too_long}`
fact carrying the offset, the byte count and a hash of what was skipped, and the
cursor advances by exactly what was read — so a line of any length is consumed in
`ceil(length / 4MiB)` polls instead of never. A short trailing fragment is still
waited for, because that is a line Claude is in the middle of writing.

A line that is not decodable JSON is also a fact now
(`error{transcript_line_unreadable}`), rather than being dropped in silence
*after* the cursor advanced past it. Valid JSON in a shape this build does not
map is still skipped without comment: the transcript format grows, and an
unrecognised entry is not damage.

## Build identity

A version number moves at releases and at capability bumps (every
`PROTOCOL_MINOR` change moves it — see RELEASING.md), but never on ordinary
commits — so between those moments it cannot answer "am I running current
code?", and two different builds happily share one number. Every shipped binary therefore also embeds the git commit
it was built from, and whether that tree was clean when it was built:

```
codeconnect 0.3.0 (227f6d4e1791)         # clean build of that commit
codeconnect 0.3.0 (227f6d4e1791-dirty)   # built from an edited tree
codeconnect 0.3.0 (build unknown)        # built outside a git checkout
```

`daemon status` shows the running daemon's build the same way. On every
`codeconnect claude` launch, the installed version is compared against the
newest GitHub Release, and one line can appear:

```
CodeConnect update available · 0.2.0 → 0.3.0
```

It ends with the one fix: `codeconnect update`, which downloads that release,
proves it is signed by CodeConnect's Apple team, and replaces all three
binaries in a single directory exchange — so an interrupted update leaves the
complete old set or the complete new one, never a mixture. A failed check
renders as silence: an advisory that can be wrong is worse than none. The
`update_check` config switch disables the background check; identity stays
visible through `--version` and `daemon status`.

## Upgrading

Three things behave differently on a Mac that has been running for a while.

* **A `ws_bind` onto a LAN needs a certificate, or your word for that network.**
  Plaintext is admitted only where the bytes are private already, so a Mac that
  binds a LAN address and has never obtained a certificate refuses every phone
  it was serving before — and logs that refusal, with both ways out of it, as it
  starts. The ways out are a certificate for a name that resolves to the bound
  address — its `<name>.crt` and `<name>.key` dropped in `~/.codeconnect/tls/`
  and the name set in `tls_hostname`, since `tailscale cert` issues for this
  node's MagicDNS name and nothing else, and that name points at the tailnet
  interface this listener is not on — or `"ws_allow_plaintext": true`, and
  [TLS](#tls) has the addresses each covers. Failing closed is the point: the
  credential a paired phone holds is a bearer token, and an address nobody can
  vouch for is not a place to put one by default. A daemon with no `ws_bind` set
  is unaffected — it binds this node's tailnet address, where WireGuard has
  already encrypted the path.
* **SSH is retired.** The Terminal tab rides the same paired connection as the
  rest of the app, so CodeConnect never installs a key. Taking one back is what
  it does do: the daemon sweeps `~/.ssh/authorized_keys` before it serves
  anything. It sweeps again on every `codeconnect revoke <device>`,
  unconditionally and including on a device that was already revoked, so
  re-running that command is the operator's retry for a sweep an unwritable
  `~/.ssh` defeated earlier — and the only retry there is short of restarting
  the daemon. What it removes is an entry matching the legacy format — a
  `# codeconnect:<device id>` marker comment with a bare three-field
  `ssh-ed25519` line carrying the same id directly beneath it — and it matches
  on that shape, because nothing in a file's bytes says who wrote them. Both
  lines go or neither does: a key line reached any other way stays, and so does
  a marker with no such key under it, since removing the label while leaving the
  key would strand a working grant with nothing left in the file to say whose it
  is. No other line is altered and key material is never examined. Where it
  removes nothing it leaves a grant standing if there is one, and each such
  outcome is a warning naming what to delete rather than a daemon that refuses
  to start or a revocation that reports failure: no absolute `$HOME` to resolve
  the path against, a file that cannot be read, a replacement that cannot be
  written in its place, a file another program rewrote or a symlink it repointed
  while the sweep was working — retried, then declined, because a copy read
  before that write is not a thing to rename over it — and tagged lines that are
  not that whole pair. A phone
  on an older version of the app reports the key as "not installed" and sends
  its owner here to run `codeconnect pair --ssh` or `codeconnect ssh-revoke`;
  both explain that they are retired and exit 1, rather than leaving that reader
  at an unrecognised command having done nothing wrong, and both print the
  search below. The remedy is to update the app — and then to settle the file
  yourself, because a revocation sweeps it on best effort and reports nothing
  about what it took. Run `grep -n codeconnect: ~/.ssh/authorized_keys`: nothing
  printed, or `grep` reporting no such file, means nothing there carries the tag
  — which is not the same as nothing being left, since a key line whose comment
  field does not carry one prints nothing and stays, and the file records who
  wrote no line. An `ssh-ed25519` line in that output is a grant still standing:
  under its own marker it is one a sweep has not reached yet, and without one it
  is a line no sweep will ever take, since the pair is the only shape this
  daemon is willing to act on — delete it and its `# codeconnect:` comment
  together, both halves or neither.
* **A rollback keeps what a newer build added to the credential tables.** The
  migration that clears the columns this project retired is drop-only: one
  `ALTER TABLE … DROP COLUMN` per retired column — `devices.ssh_key_installed`,
  `devices.ssh_fingerprint` and `pairing_codes.allow_ssh` — named one at a time,
  never every column this build does not recognise. Nothing is rebuilt, no table
  is dropped and no row is deleted, so a column a newer build added is never so
  much as looked at: it survives a run of an older one and is still there on the
  way back up. It survives the awkward case too, where the same table *also*
  still carries a retired column, since dropping that one column reproduces
  nothing else — every other column keeps the type and the constraints it was
  declared with, and every row keeps its values. SQLite refuses the drop when
  the column is a `PRIMARY KEY`, is `UNIQUE` or is indexed, or is named by a
  view, a trigger, a `CHECK` or a generated column. That refusal is logged as a
  warning naming the table, the column and what SQLite said, and the daemon
  carries on: the column stays where it is and this build serves normally with
  it there, since nothing it reads names it. Minting a pairing code still works
  with a leftover `allow_ssh` in place, because that `INSERT` names the column
  explicitly with a literal `0` for as long as it is present. To be rid of the
  column, drop whatever depends on it — the logged error names it — and restart.
  Nothing is written at all when nothing is retired, which is every boot after
  the first. Two writes run on every open, outside those tables and outside that
  guarantee. Adopted sessions — the `claude:*` rows cc-hook mints for runs this
  daemon did not launch — have `tmux_session` and `tmux_socket` cleared, because
  empty is the honest answer for a process this daemon cannot locate; a location
  a newer build recorded there does not survive the trip down. And
  `user_version` is stamped with this build's number. Nothing here ever reads it
  back, so an older daemon opens a newer database, never compares the two, and
  leaves its own number behind.

## The LaunchAgent

`codeconnect daemon install` writes `~/Library/LaunchAgents/com.codeconnect.ccd.plist`
with `RunAtLoad`, `KeepAlive{SuccessfulExit: false}` and a five-second
`ThrottleInterval`, then bootstraps it. Measured over three soak runs (15 kills):
`kill -9` on the daemon is recovered in 0.4–4.7s — the spread is
`ThrottleInterval`, which is launchd's floor between restarts and the price of
not letting a crash-looping job spin a core. Every session survives it: they live
in tmux, and their supervisors reconnect (3.1–4.5s observed).

Paths are resolved **at install time**, because launchd hands a process no shell
`PATH` at all. The plist names the ccd binary absolutely and carries a `PATH`
built from wherever `tmux`, `git`, `tailscale` and `claude` actually are on this
machine; the daemon separately resolves those from absolute candidate lists in
code, because the two failures look identical from the outside.

Installing stops whatever ccd is running, its own job included — the plist
cannot be replaced under a live one. A daemon started by hand gets SIGTERM and a
wait for the socket; a managed one gets `launchctl bootout` and the same wait.
The wait is not cosmetic: two daemons briefly sharing one `~/.codeconnect` is
the only situation where both could open the event log at once, which is the one
state the schema migration must never run from. Nothing is lost either way — the
sessions are in tmux and the log is in SQLite.

`--no-takeover` refuses instead of stopping anything; `codeconnect daemon restart` picks
up new binaries without rewriting the plist. A daemon too old to answer
`daemon_info` is identified through `lsof` on its socket rather than guessed at.

`codeconnect daemon uninstall` removes the plist only once `launchctl bootout` has
actually succeeded. An unrecognised failure there means the job may still be
loaded, and deleting its definition would leave a daemon that nothing can
manage, restart or stop.

The daemon rotates its own logs (`ccd.out.log`, `ccd.err.log`, capped by
`log_max_bytes`, one `.1` generation kept). It has to be the daemon: launchd
holds those descriptors open, so an external rotator that renames the file would
leave launchd appending to an unlinked inode — the log stops growing, the disk
keeps filling, and nothing says so.

## Soaking it

`soak/` is a repeatable gauntlet against the **installed** daemon — the same
socket the hooks use, the same database the phone reads.

```sh
soak/run.sh              # start a real session, run every scenario, tear down
soak/run.sh --keep       # …and leave the session running
soak/run.sh --session cc-1 -- wsflap
```

| Scenario | What it attacks |
|---|---|
| `kill` | `kill -9 ccd` five times during traffic; asserts `max_seq == count` and no `(source, source_event_id)` duplicates afterwards, and that the supervisor re-attached. |
| `hookstorm` | The same hook payload 50× concurrently; asserts exactly one event and no burnt sequence number. |
| `answerstorm` | 20 concurrent answers for one `request_id`; asserts one applied and nineteen duplicates carrying the *original* outcome. |
| `wsflap` | 30 connect/subscribe/disconnect cycles from spread watermarks; asserts every replay is contiguous and monotonic. |
| `tailtorture` | Truncates, rewrites and tears a transcript under the tailer; asserts the cursor recovers with no duplicate ingest. |
| `ingestkill` | `kill -9 ccd` timed *inside* the tail poll, five times; asserts every transcript line written is in the log exactly once. The loss it hunts is silent by construction — no gap in `seq`, no duplicate — so only "is every line I wrote there?" can see it. |
| `commitorder` | 40 concurrent commits on one session while a socket watches; asserts the socket receives strictly successive seqs and gets no resync marker. The database is consistent either way, so the connection is the only vantage point the defect is visible from. |
| `commands` | `/status`, `/usage` and `/cost` through the phone's own path; asserts each comes back `composer_recovered` carrying the pane the daemon saved while the Mac's view was up, and that an ordinary send lands **immediately** after each one with nobody touching the Mac. The last clause is the whole feature: these three commands take Claude's composer away, and while it is gone the interlock refuses every send, so a phone that opens one and cannot close it has locked itself out of its own session. |

`run.sh` restarts the daemon after installing, and `ccsoak` refuses to run
against a daemon whose `protocol_minor` is below the one the harness was built
with. Both exist for the same reason: `install.sh` replaces the binaries on
disk, launchd keeps executing the process it already started, and a gauntlet
that attacks the previous build reports passes for code that was never loaded.

It connects over `ws://127.0.0.1:8787` rather than the tailnet address. Reaching
one's own tailnet IP leaves the machine through the utun interface, where a
third-party network filter gets a vote — measured here, Little Snitch cannot
identify a freshly `cargo build`-ed binary and holds its connection open and
silent for 70 seconds before a reset, while leaving loopback alone. Set
`CCSOAK_HOST` to prove the tailnet path instead, and `"ws_loopback": false` to
turn the loopback listener off.

## Pairing

`codeconnect pair` asks the daemon for a single-use 8-character code (5-minute TTL,
stored hashed) and prints it as a QR alongside the JSON the app decodes:

```json
{"v":1,"host":"<magicdns-name>","port":8787,"code":"ABCD2345"}
```

The app connects and sends `hello{pairing_code}` with no token. The daemon
validates and consumes the code, mints a 256-bit per-device token, and returns
it once in `hello_ack{device_token}`; the app keeps it in the Keychain. Only the
token's hash is stored here, so a leaked database cannot be replayed as a
credential.

### Both ends refuse what is unreachable by definition

`ccd` resolves its address **once, at startup**, and every code minted afterwards
carries whatever it resolved. launchd starts the daemon at login and does not wait
for the Tailscale tunnel, so losing that race leaves the daemon bound to loopback —
and it used to print a perfectly scannable QR containing `127.0.0.1`, exit 0, and
add a note saying Tailscale encrypts the link. It does not; there is no tailnet link
in that state. Every visible sign was success and only the code was dead.

Two checks now make that state impossible to hand to a phone:

* A daemon never advertises a host it knows its own listener does not answer at.
  Bound to loopback it advertises **loopback**, never a MagicDNS name. Bound to
  an address it was given — a LAN address, say — it looks the name up first, and
  an answer putting some other address behind it refutes the name outright: the
  QR carries the bind address instead, with no certificate obtained. A resolver
  that fails, answers empty, or does not answer inside three seconds has refuted
  nothing, and that case is settled by whether a certificate was obtained for
  the name — the full rule is under [TLS](#tls). The endpoint and the socket are
  one claim, and a name that resolves somewhere nothing is listening is worse
  than an address that is visibly local: the address is visibly wrong, and the
  name is not.
* `codeconnect pair` refuses to print a code whose host is loopback, link-local or
  unspecified, and names the recovery: bring Tailscale up, then
  `codeconnect daemon restart`.

The phone applies the same rule to a scanned code, offline, before it dials — see
`PairingQRPayload.unreachableHost`. Both sides reject only what is *definitionally*
unreachable; neither demands a `100.64/10` address or a `.ts.net` name, because a
custom DNS name or a deliberately pinned `ws_bind` are legitimate. A name neither
side can settle — one this Mac's resolver could not answer for, carried because a
certificate was obtained for it — is past what either check can see, and the
daemon says in its log that the name went out unconfirmed.

The code's alphabet omits `I`, `O`, `0` and `1`, so reading it aloud when the
camera fails has no ambiguous characters. The QR is drawn with explicit
black-on-white ANSI so it scans under a dark terminal theme, where art that
relies on the default foreground colour comes out inverted.

The static `codeconnect token` credential stays valid alongside device tokens. Deleting
`~/.codeconnect/token` is the only thing that retires it.

`codeconnect revoke` takes effect on connections that are **already open**, and takes
effect *immediately*. Three mechanisms, in order of how fast they act:

1. **A cancellation is published the instant the token is revoked.** Every open
   socket is subscribed to it and closes the moment its own device id appears.
   Without this, an *idle* phone — one holding a subscription and sending
   nothing — kept receiving the live event log until its next keepalive, up to
   30 seconds after the operator revoked it.
2. **Every message re-checks**, so a device that reconnects into the gap is
   still refused.
3. **The keepalive tick re-checks**, as the backstop for a connection that was
   not listening when the cancellation went out.

A revoked device gets one `error{code: "revoked"}` and is disconnected.

**The revocation check fails closed.** A database error while answering "is this
device still active?" closes the connection rather than admitting it. It used to
be read as *active*, on the reasoning that a SQLite hiccup should not drop every
live connection — but this is an authorisation decision, and one that cannot be
made must not be resolved in the caller's favour. The costs are asymmetric: a
false close is a reconnect, a false open is a revoked phone still reading the
event log and answering approvals. The reconnect authenticates against the same
database, so a genuinely broken store denies access either way rather than
grandfathering whoever was already inside.

**The device token is the whole of what this daemon grants a phone**, so
revoking it takes away everything it issued: the terminal rides the same
connection and dies with it, and nothing minted here outlives the token.
Revocation closes open connections rather than waiting for the next reconnect,
and a failure to revoke is reported as an error rather than as a partial
success.

**Revocation reaches `~/.ssh/authorized_keys` too, as far as a best-effort sweep
reaches.** An earlier release appended the phone's public key there, and that
grant lives entirely outside this database — no row here records it, and
withdrawing the token does nothing to it, which is why a revocation also runs
the sweep the daemon runs at startup ([Upgrading](#upgrading)): the whole file
rather than the named device's entries, and unconditionally, so re-running the
command retries a sweep that failed earlier. What it cannot do is promise. Five
paths remove nothing and warn instead — no absolute `$HOME`, a file that cannot
be read, a replacement that cannot be written, a file that changed under the
sweep (retried, then declined rather than overwrite whoever else was writing),
and a tagged line that is not the
marker-and-key pair it recognises — and a key outside
`$HOME/.ssh/authorized_keys`, or outside that shape, is one it never sees at
all. Revoking a phone can therefore still leave it a shell. On a Mac that ever
ran `codeconnect pair --ssh`, confirm the outcome rather than assume it:

```sh
grep -n codeconnect: ~/.ssh/authorized_keys
```

Nothing printed, or `grep` reporting no such file, means nothing there carries
the tag — which is not the same as nothing being left, since a key line whose
comment field does not carry one prints nothing and stays, and the file records
who wrote no line. An `ssh-ed25519` line in that output is a grant no revocation
took back: under its own marker, one a sweep tried and could not finish or has
not run against since it appeared; without one, a line no sweep will ever claim.
Delete it and its `# codeconnect:` comment together.

The static token has no revocation record — delete the file and restart to
retire it.

**Pairing attempts are rate-limited.** A pairing code is 2^40 and lives five
minutes, so this is not what stops a guess; it is what stops an unbounded
*number* of guesses, because "you cannot try enough of them in five minutes" is
only true if something is counting. Twenty failures inside five minutes
(`pairing_max_attempts`, `pairing_window_secs`) closes the window for everyone
until they age out, and logs `PAIRING RATE LIMIT` — a human pairing a phone
types one code, so reaching it is a signal. Successful pairings are not counted,
so a household adding devices can never lock itself out. The peer is told
nothing about *why* it was refused: every failure gets the same opaque message,
because telling a caller it is being rate-limited hands it the pacing
information it needs.

## TLS

At startup the daemon settles on one name — `tls_hostname` where that is set,
otherwise the MagicDNS name from `tailscale status --json` — and asks for that
name's certificate. It looks in `~/.codeconnect/tls/` for `<name>.crt` and
`<name>.key` first, and shells out to `tailscale cert` only when what is there
is missing, unparseable, or has fewer than `cert_refresh_days` (30) of validity
left — read out of the certificate's own `notAfter`, not from a sidecar note, so
a certificate replaced by hand is still assessed on its merits. One found on
disk and one `tailscale cert` has just written are served identically.

**A certificate `tailscale cert` will not issue goes in that directory by
hand.** It issues for this node's MagicDNS name and nothing else, so any other
name — and a LAN `ws_bind` needs one, because MagicDNS points at the tailnet
interface — has to be obtained however that name is served, written to
`~/.codeconnect/tls/` as `<name>.crt` and `<name>.key`, and named in
`tls_hostname`. That is the whole of the setup: the directory is read before
Tailscale is asked for anything. Replace the file before it comes inside
`cert_refresh_days` of expiry, because at that point the daemon tries to renew
it, `tailscale cert` refuses a name that is not this node's, and the failure
drops the certificate rather than falling back to the one on disk — logged as
`tls: no certificate for <name>`, and on a LAN bind a listener that then refuses
every connection.

**Clients must connect by name, not by address.** The certificate's SAN is a DNS
name; `wss://100.x.y.z:8787` cannot validate against it. That is why the QR
carries the MagicDNS name rather than the tailnet IP, and why it goes on carrying
that name when no certificate could be obtained: the name survives a Tailscale IP
change, which a literal does not, and it is what the phone will need once one is.

**A name known to miss the listener is never carried.** On a tailnet bind the
MagicDNS name maps to the bound address by construction — both describe the same
node — and on `0.0.0.0` the listener is on every interface this Mac has, so every
name of this Mac reaches it. Neither is looked up. On any other `ws_bind` nothing
about the bind implies anything about where a name points, so the daemon resolves
it, and there are three answers rather than two:

* **It resolves to the bound address.** Carried, and served as `wss://` if a
  certificate can be had for it. A `tls_hostname` that really does point at your
  LAN address is the case this exists for.
* **It resolves somewhere else.** Refused. The QR carries the bind address and no
  certificate is obtained, because a certificate for a name no phone reaches this
  daemon at protects a connection nobody opens. The refusal is logged with what
  was refused, why, and how to serve `wss://` on that bind instead.
* **Nothing answered** — the lookup failed, came back empty, or ran past its
  three-second budget. Silence refutes nothing, so the certificate settles it.
  With one, the name is carried and the daemon logs that it is advertising a name
  it did not confirm: validating a certificate is the only reason a phone dials a
  name rather than an address, so a name with one behind it earns its place
  unconfirmed. Without one, no phone needs the name, and the QR carries the
  address this daemon knows it listens on.

Silence is read as refusal in exactly one place: the MagicDNS name beside a bind
Tailscale never hands out — a private or link-local address, or a unique-local
one outside Tailscale's own `fd7a:115c:a1e0::/48`. That name maps to this node's
tailnet address and to nothing else, so the two are known to disagree before any
lookup and a silent one adds nothing. A `tls_hostname` the operator set is theirs
to vouch for and is not overruled the same way; split-horizon DNS, where the name
answers for the phone and not for the Mac serving it, is the ordinary reason a
lookup here settles nothing, and a daemon that refused on it would take a working
`wss://` listener down to one that serves nobody. A QR that resolves perfectly
and reaches nothing looks exactly like success from every side.

Both schemes share port 8787. The listener peeks the first byte — a TLS record
always starts `0x16`, an HTTP request never does — so a phone that has not been
updated keeps working. Set `tls_required: true` once every client speaks
`wss://` to refuse plaintext.

If `tailscale cert` fails (commonly: HTTPS Certificates are not enabled for the
tailnet, under *admin console → DNS → HTTPS Certificates*), the daemon logs why,
reports `capabilities.tls: false`, and serves `ws://` wherever plaintext is
private. On loopback and on this node's tailnet address what is lost is the
certificate and not the confidentiality — the bytes never leave the machine, or
WireGuard encrypted the path before it touched a network. Where
`ws_allow_plaintext` has opened a LAN bind as well, both are lost: Tailscale
carries no part of that link, nothing else encrypts it, and that is the trade
the key exists to make.

**Plaintext is admitted only where the bytes are private already.** The
credential is a bearer token, and a connection carries the event log and the
approval path — so `ws://` is served on loopback, and on this
node's own tailnet address, where WireGuard has encrypted the path before it
touches a network. An explicit `ws_bind` onto a LAN is neither, and with no
certificate that listener refuses every connection before the WebSocket
handshake and says so in `ccd.err.log` as it starts. That line names both ways
through — a certificate, and the key below — and offers the key only where it
would work when followed.

**`ws_allow_plaintext: true` is the one key that says otherwise** — an operator
who knows the network their chosen address is on and accepts what crosses it in
the clear. It buys back every capability except one: a connection admitted this
way is advertised `terminal_pty: false` and its `terminal_attach` is refused
`not_authorised`, because "I vouch for this network" and "these bytes are
unreadable on it" are different statements, and a live shell's keystrokes are
only sent on the second. The listener is `OperatorAllowed` rather than
`TrustedPath` internally, which is exactly that distinction: admissible, not
private. It is honoured only for a bind on the local network: loopback,
private IPv4, IPv4 link-local, IPv6 unique-local (`fc00::/7`) or IPv6 link-local
(`fe80::/10`). It is refused for `0.0.0.0` and `::`, where nobody can enumerate
in advance which interfaces the token has just gone onto, and for a public
address, where plaintext is private on no hop of the way; both refusals have the
same way through, which is to bind this Mac's own LAN address. Honoured, it is
logged at startup, because a bearer token crossing a network in the clear is a
fact an operator has to be able to find later. `tls_required` still wins: an
operator who has declared that every client speaks `wss://` is not also asking
for an exception, and honouring one would announce plaintext that the admission
gate then refuses.

## What the phone can ask for

`hello_ack.capabilities` reports `tls`, `tls_active`, `diff`, `risk_class`,
`session_uid`, `send_text_idempotent`, `prompt_identity`, `push`, `send_text`,
`capture`, `delete_session` and `test_push` so the app disables affordances it
does not see advertised instead of failing at tap time.
`hello_ack.protocol_minor` is the additive feature level: `>= 1` means
`TurnComplete`, `get_diff`, pairing and `risk_class` are all present; `>= 2`
means every session and event carries a `session_uid`, every message that names
a session accepts one, and `answer` may carry `session_id` to scope itself to a
run; `>= 3` means `send_text` is an idempotent mutation and approval cards carry
a prompt generation; `>= 4` adds the `protocol_mismatch` and `message_too_large`
error codes, the truncated-event placeholder, and the transcript tailer's two
`error` payload shapes; `>= 5` means `lifecycle` is reconciled against tmux
rather than merely remembered, so `live` is proven rather than unrefuted, and a
derived `session_end` carries a `reason`; `>= 6` adds `register_push`, the
phone telling the daemon where to send a notification; `>= 7` adds
`delete_session`; `>= 8` through `>= 10` cover the diff, the composer and the
native slash-command adapters; `>= 11` adds `SessionSummary.project_label` —
the daemon resolving what a run is *called* (the last component of its working
directory), so that every surface that names a run, including a notification
the phone cannot compose for itself, uses one string. See
`protocol/src/lib.rs` for the authoritative ledger.

**`delete_session` is the only destructive verb a phone has.** It names the run
by `session_uid` and never by `session_id` — a tmux name is handed to the next
run, so it is not an identity a destructive request may be pointed at. For a
hosted run the daemon refuses anything whose lifecycle is not `exited`, and
anything it still holds a supervisor or an open approval for. An **adopted**
run — empty `tmux_session`; CodeConnect never launched it, so no probe can
ever prove it ended — is deletable at any lifecycle, and its deletion also
stops observation of that conversation until a new session start re-adopts it.
A refusal is a `delete_session_result`, not an error. It deletes the daemon's own record only:
Claude Code's transcripts live under `~/.claude/projects` and are untouched, so
`codeconnect claude --resume <id>` still works afterwards.

**`protocol_version` is now enforced.** A client on a different *major* is
refused with `error{code: "protocol_mismatch"}` and disconnected, before its
credential is even looked at. It used to be logged as a warning and then handed
the full event log and the approval path — a peer that by the definition of the
number has a different idea of what these messages mean. The majors have not
moved, so nothing that works today stops working; checking before the token also
means an incompatible peer cannot use the handshake to probe credentials.

* **`send_text{session_id, text, request_id?, payload_hash?, submit}`** is a
  durable idempotent mutation from minor 3. `payload_hash` is
  `sha256` over length-prefixed `(session_id, submit, text)` — the target and
  the submit flag are in it, so a captured frame cannot be replayed with new
  text under an id the ledger already trusts. A retry with the same pair replays
  (`status: "duplicate"`); a retry with the same id and different material is
  refused. The body is capped at 8KB. Both new fields are optional so a client
  on minor 2 keeps working — such a request is applied at most once per attempt
  but cannot be made idempotent across retries, and the daemon says so in its
  log rather than pretending otherwise.
  **`require` is now ignored.** The client used to nominate the needle that
  authorised its own keystrokes (`PromptPresence::AnyOf`), which is not a safety
  check; the server picks the interlock (composer presence, with the operator's
  configured needle overrides). The field still decodes so an older client's
  message parses.
  `SendTextResult` gains `duplicate` and `indeterminate`, neither of which can
  reach a client that did not send a `request_id`.

* **`get_diff{session_id}`** → `diff{session_id, unified, truncated, captured_at, note}`.
  `git -C <session cwd> diff HEAD` plus a listing of untracked file names, capped
  at 512KB. The client names a session, never a path. A non-git directory returns
  an empty diff *with a `note`* — an empty diff and "this was never a repository"
  must not look the same on screen.

  The cap is applied **at the read**, not after it. `Command::output()` reads a
  child to EOF into memory and only then lets the caller trim, so the cap bounded
  what was *sent* rather than what was allocated: one generated file — a
  lockfile, a minified bundle, a dump committed by accident — made a single
  `get_diff` an arbitrary allocation. Now at most `cap + 1` bytes are read (the
  extra byte is what distinguishes "this is all of it" from "there is more"), the
  child is **killed** rather than politely drained, stderr has its own 8KB
  ceiling, and the deadline kills the process instead of abandoning it. Each pipe
  is owned by its own reader, which is what stops the cap on one stream
  deadlocking the other.

  The invocation carries `--no-ext-diff --no-textconv -c core.fsmonitor=false`.
  Those are not tidiness: `diff.external`, a `textconv` filter and `fsmonitor`
  all name a **program git will execute**, and `textconv` is attached through a
  `.gitattributes` that lives *in the repository*. This path is reachable from
  the phone, so without them, asking for a diff of a working tree somebody else
  prepared would run their program as the daemon's user. Observing a working tree
  must not be a way to execute anything.
* **`EventKind::TurnComplete`** is emitted on the Stop hook. `SessionEnd` now
  means only what it says: the supervisor exited or the tmux session is gone.
* **`risk_class`** rides on every `approval_request` card as
  `risk{class, matched_pattern}`. `high` for destructive shell patterns, `low`
  for pure read tools, `medium` for everything else — including anything
  unrecognised, because "we do not know this tool" is not evidence of safety.
  Matching is token-based, so `confirm -rf` is not `rm -rf` and `2>/dev/null` is
  not writing to a device node. It is a rendering hint for a human, never a gate.
* **`ResolvedBy::Local`** — when a prompt is answered at the Mac's keyboard the
  daemon notices and emits `approval_resolved{resolved_by: "local"}`, so the
  phone shows "answered at the keyboard" instead of a rejection. If a tool result
  arrives the decision is observed; if the prompt merely left the screen the
  decision is a guess and is flagged `inferred: true`.
* **`ResolvedBy::Superseded`** — a newer prompt replaced this one before it was
  answered. Nothing was typed, so a tap on it comes back `rejected` with that
  reason, not `duplicate`.
* **`AnswerOutcome.indeterminate`** — the decision is known and whether it
  reached the agent is not, because the daemon stopped between typing it and
  recording it. Distinct from `inferred`, which is uncertainty about the
  *decision*. Never retried.
* **`ApprovalCard.generation` / `identity_bound`** and the
  `approval_prompt_bound` event — see [Prompt identity](#prompt-identity).

### The live terminal (minor 13)

`terminal_attach{attachment_id, session_uid, cols, rows, output_credit}` opens a
tmux control-mode carrier on a hosted session, answered by `terminal_attached` or
`terminal_closed`. Bytes ride base64 inside the JSON frames under a credit window
in each direction; nothing streams before the attach is verified.

**`terminal_attached` carries the ceilings**, `max_chunk_bytes` (16 KiB) and
`max_outstanding_credit` (256 KiB), beside the initial `input_credit`. A client
that hard-codes them instead enforces this daemon's numbers for the life of the
install, and the first daemon to raise either one kills the terminal of every
phone already out there. No `terminal_output` exceeds `max_chunk_bytes` decoded —
including the screen an attach paints, which is one chunk the forwarder splits at
the bound rather than a frame the phone's grant happened to size.

**A terminal needs a private transport.** `terminal_pty` is advertised only to a
paired device on a connection that is `wss://`, loopback, or this Mac's own
tailnet address, and `terminal_attach` enforces the same rule independently of
what the client was told. See [TLS](#tls).

**One terminal per session, and a later attach takes it over.** Attaching to a
session that already has one closes the incumbent with code `superseded` and then
opens the new one — on the same connection or from another, and whichever device
asked last wins. Refusing the newcomer, which is what the lease used to do, locks
a phone out of its own session for as long as a dead connection holds the lease:
iOS backgrounds the app, the socket dies with no FIN, and nothing frees it until
TCP notices. The takeover waits up to five seconds for the displaced carrier's
disposable tmux client to be reaped — two clients on one session must never
overlap — and refuses with `session_busy` if it is not, which is a different code
from `attachment_limit` precisely because it is worth retrying and the Mac's
global cap of eight is not.

**Frames behind an attach wait for it.** An `input`, `resize`, `credit` or
`detach` that arrives while the attach is still opening is queued in arrival
order (bounded at 128 frames; input's own credit window makes that unreachable
for a client honouring the protocol) and applied the moment the terminal exists.
Every *other* message — an approval answer, a ping, a subscribe — is read and
handled throughout. It used to be that no message at all was read during an open,
which cost an approval up to the twenty-second attach deadline and could make the
user miss its `respond_by`.

## How it fits together

```
terminal tab ──exec──> tmux -L codeconnect (session cc-1) ──> real claude
                              │                                   │
                              │                            generated --settings
                              │                                   │ hooks
                              │                                   ▼
                     codeconnect supervise (detached,                   cc-hook
                      own process group,                          │
                      PPID 1 once the tab closes)                 │ unix socket
                              │                                   │
                              └──────────► ccd ◄──────────────────┘
                                            │
                                   SQLite (source of truth)
                                            │
                              wss:// tailnet (ws:// fallback) ──► iPhone
```

`ccd` is never an agent's parent. Both the supervisor and the hooks **connect
out** to it, so `kill -9 ccd` costs a reconnect and nothing else.

## Configuration — `~/.codeconnect/config.json`

Every field is optional. The defaults are what the daemon is validated against.

| Key | Default | Meaning |
|---|---|---|
| `ws_port` | `8787` | WebSocket port. |
| `ws_bind` | tailnet IP | Explicit bind address; otherwise `tailscale ip -4`, else loopback. The QR host is never one known to point somewhere else: bound to loopback the daemon advertises loopback and `codeconnect pair` refuses to print a code, and bound off the tailnet it drops a name this Mac's resolver puts at another address — see [Pairing](#pairing) and [TLS](#tls). |
| `ws_loopback` | `true` | Also listen on `127.0.0.1`, for tools on this Mac. The token is still required. |
| `gate_hook` | `"PermissionRequest"` | Which hook waits for the daemon. `"PreToolUse"` or `"none"` also valid. |
| `hold_ms` | `0` | How long to hold the gate hook for a phone answer. `0` = never hold. |
| `unreachable_ask` | `false` | When the daemon is unreachable, make PreToolUse return `ask` with our reason. Renders the reason to the operator, at the cost of prompting on every tool call. |
| `input_box_needles` | built-in | Whitespace-insensitive needles proving the composer is ready. |
| `permission_prompt_needles` | built-in | Needles proving a permission prompt is on screen. |
| `send_keys_delay_ms` | `120` | Pause between typing text and pressing Enter. |
| `tmux_status` | `false` | Show tmux's status bar inside the session. |
| `claude_bin` | auto | Explicit path to the real `claude`. |
| `tls` | `true` | Find a certificate for the QR host — cached, dropped in `~/.codeconnect/tls/` by hand, or from `tailscale cert` — and serve `wss://`. Falls back to `ws://` rather than refusing to start. |
| `tls_required` | `false` | Refuse plaintext. Turn on once every client speaks `wss://`; both share one port until then. |
| `ws_allow_plaintext` | `false` | Serve `ws://` on a `ws_bind` whose privacy only you can vouch for. Honoured on a local-network address; refused on `0.0.0.0`, `::` and public addresses. `tls_required` wins over it. See [TLS](#tls). |
| `tls_hostname` | auto | Override the MagicDNS name used for the certificate and the QR host. Dropped where this Mac's resolver puts the name at an address other than the bind; a resolver that says nothing is not a refusal, and the certificate settles that case. See [TLS](#tls). |
| `cert_refresh_days` | `30` | Re-issue when the certificate has fewer days of validity left. |
| `pairing_ttl_secs` | `300` | Pairing code lifetime. Clamped to 30…3600. |
| `local_resolve` | `true` | Detect approvals answered at the Mac's keyboard. |
| `local_resolve_grace_ms` | `3000` | How long to wait before believing the absence of a prompt. The hook fires *before* the TUI draws, so a shorter grace would resolve every approval as local. |
| `local_resolve_poll_ms` | `2000` | How often to re-check the pane while an approval is open. |
| `diff_max_bytes` | `524288` | `get_diff` cap; larger diffs come back with `truncated: true`. |
| `diff_timeout_ms` | `10000` | Per-`git`-subcommand ceiling. |
| `git_bin` | auto | Explicit path to `git` (launchd has no shell PATH). |
| `fsevents` | `true` | Watch transcript directories with FSEvents. The poll below keeps running either way. |
| `log_max_bytes` | `8388608` | Cap on each launchd log; one `.1` generation is kept, so the ceiling is twice this per stream. |
| `log_rotate_secs` | `300` | How often the cap is checked. |
| `ws_max_connections` | `64` | Live tailnet WebSocket connections. See [Bounds](#bounds). |
| `ws_max_per_peer` | `32` | How many of those one remote address may hold. |
| `ipc_max_connections` | `128` | Live connections on the local unix socket. |
| `ipc_write_queue` | `1024` | Frames one IPC connection may have queued for writing before the read loop feels backpressure. |
| `pairing_max_attempts` | `20` | Failed pairing attempts allowed per window. `0` disables the limiter. |
| `pairing_window_secs` | `300` | The window those attempts are counted over. |

The needle lists are configuration rather than constants on purpose: Claude
Code's prompt wording is the most churn-prone thing this depends on, and it must
be fixable without a release.

## Filesystem boundary

Everything under `~/.codeconnect` is owner-only, and the boundary is **repaired
at startup** rather than only established at creation.

* Directories are created `0700` by `mkdir(2)` itself, not chmod'ed afterwards,
  so no window exists in which they are readable.
* The static token, the database, its `-wal` and `-shm` sidecars, the config and
  the launchd logs are `0600`.
* An installation made before this existed already has the loose modes on disk,
  so every path above is re-checked at each start. A boundary enforced only on
  new state protects nobody who has already run the daemon.

SECURITY.md has always said the database is "protected by file permissions". It
was not: every directory was `create_dir_all` under the process umask, and the
macOS login default of `umask 022` produced a `0755` state directory holding a
world-readable event log — which is a verbatim copy of every transcript line an
agent has produced, secrets included.

The database is subtle in one way worth knowing. SQLite creates the `-wal` and
`-shm` files itself and copies the **main database's mode** onto them, and the
`journal_mode=WAL` pragma is what creates them — so the database is made `0600`
*before* the connection is opened, and the sidecars inherit it. Tightening
afterwards would leave the newest, not-yet-checkpointed facts in the log
readable.

`chmod` is never applied through a symlink: every repair stats with
`symlink_metadata` first and refuses rather than changing some other file's mode.

## The database and the runtime

SQLite is synchronous and does real disk I/O, so calling it from an `async fn`
blocks a Tokio worker. With `rt-multi-thread` there are as many workers as
cores, and the `busy_timeout` is five seconds — five seconds of a completely
dead daemon, from a database another process happened to be holding. Two
changes:

* **One writer, a pool of readers.** There used to be a single
  `Mutex<Connection>` for everything, which made every read wait behind every
  write and reintroduced exactly the contention WAL had just removed. A 4MB
  transcript batch held that lock for its whole duration, stopping every replay,
  every session listing and every revocation check on the machine. Now writes
  take one dedicated connection — which is also what keeps `BEGIN IMMEDIATE`
  cheap, since there is no in-process writer to wait out — and reads take
  whichever of four read-only connections is free.
* **Every call runs on the blocking pool.** `ccd::db::Db` is the async handle
  the daemon uses; the synchronous `Store` stays synchronous because that is
  what a blocking thread wants to call. Moving *where* the work runs does not
  move *when* it is ordered: the per-session publish gate is a
  `tokio::sync::Mutex` held across the commit's await, so append-then-publish is
  the same sequence it always was, one thread further out.

A test pins this: on a single-threaded runtime, a 5ms timer scheduled during a
long commit must still fire on time. Against the old code it fired 548ms late.

## Bounds

Everything that accepts work from outside has a ceiling, and the ceilings are
derived from one shared resource rather than chosen individually.

**File descriptors.** macOS gives a launchd job a soft `RLIMIT_NOFILE` of 256.
The daemon needs roughly 20 for itself — five SQLite connections plus their WAL
and SHM handles, the listeners, the log files, the FSEvents watcher — so about
230 are left to divide. `ws_max_connections` (64) and `ipc_max_connections`
(128) total 192 and leave headroom. The split is deliberately lopsided: the
local socket carries every hook and every supervisor, and a hook that cannot
reach the daemon is an agent that stops being observable, so the *remote* side
gets the smaller share. A test asserts the arithmetic, so raising one of these
is a decision rather than an accident.

**Per peer.** The global cap alone would let one misbehaving client refuse every
other device, so no single remote address may hold more than `ws_max_per_peer`.
It is half the global cap, which guarantees room for somebody else. Generous
rather than tight for two reasons: every connection on the loopback listener
shares one peer address, so a small share would act as a global cap on all local
tooling; and a client may legitimately open several sockets at once — the soak
harness's answer storm opens twenty deliberately, because pipelining them down
one socket would not test the race it exists to test.

**Queues.** Each IPC connection's writer is fed by a bounded channel. A client
that stops reading — a `codeconnect ls` suspended with ctrl-Z, a supervisor whose process
is stopped — used to have the daemon buffer frames on its behalf without limit.
Now the read loop simply stops taking new work from a peer that is not consuming
its replies. Requests *to* a supervisor use `try_send` instead of waiting, so a
supervisor that has stopped reading is reported as `NotSent` — nothing left the
process, so nothing can have been typed and a retry is safe — rather than
stalling the answer path behind it.

**In-flight requests.** A supervisor round-trip's entry in the in-flight map is
owned by a guard, so it is removed on every exit path: a reply, a timeout, a
refused send, a dropped future. It used to be removed only by a *reply*, which
leaked one entry per abandoned request on a map that lives as long as the
session — unbounded growth on the stall path, which is the path that fires when
the daemon is already under stress.

**Frames.** Incoming WebSocket messages were already capped at 1MB; outgoing
ones now are too. `URLSessionWebSocketTask` fails the *connection* rather than
the message when one arrives over its limit, so a single oversized event
disconnected the phone — which reconnected, replayed, and hit the same event
again. An oversized event is now replaced by a same-shaped placeholder that
keeps its `seq`, so the watermark still advances and the client makes progress.

**Child processes.** `get_diff` streams at most `cap + 1` bytes from `git` and
kills the child rather than draining it; stderr is capped separately; the
deadline kills rather than abandons. See [What the phone can ask
for](#what-the-phone-can-ask-for).

## Testing

```sh
cargo test                        # the workspace suite, including fixture replay
soak/run.sh                       # the live gauntlet, against a real session
```

The daemon's own tests drive a **fake supervisor** over a screen the test
controls: it makes its decisions with the same `protocol::ipc` functions the real
supervisor calls (presence needle, then prompt fingerprint), so the interlock is
exercised rather than re-implemented. Its one addition is a split screen —
scrollback is returned only for a capture that did not ask for the visible pane,
which is how a test can tell "read the screen" from "read the history".

`ccd/src/fixture_replay.rs` runs recorded payloads from real sessions
(`fixtures/`) through the production ingest path, so a Claude Code schema change
fails the suite instead of failing silently in production.

## Known limits

* **APNs is a logging stub** until a `.p8` key exists; `hello_ack` advertises
  `push: false` so the phone can tell "not configured" from "failed".
* **A certificate from Tailscale depends on the tailnet.** `tailscale cert`
  needs HTTPS Certificates enabled for the tailnet, and issues only for this
  node's MagicDNS name. Without it the daemon serves `ws://` and says so; a
  certificate obtained elsewhere and dropped in `~/.codeconnect/tls/` is the way
  round it, and the only route open to a LAN `ws_bind`.
* **`ResolvedBy::Local` is best-effort by construction.** A prompt that leaves
  the pane tells us it is gone, not what was chosen; that case is reported with
  `inferred: true`. The failure mode is deliberately biased towards leaving a
  card open too long rather than clearing one the human has not answered.
* **The risk class is a hint, not a gate.** CodeConnect has no permission model;
  `risk_class` changes how a card looks, never what is allowed. The `high` set is
  an enumerated list of destructive shell patterns plus the spellings that reach
  the same place (`eval "$(curl …)"`, `` `curl …` ``, `source <(curl …)`), *not* a
  general destructiveness oracle: `git clean -fdx`, `git reset --hard`,
  `find -delete` and `terraform destroy` are `medium`, and destruction hidden
  inside a script (`npm run nuke`) is invisible to any text classifier. A miss
  degrades to the behaviour before risk classes existed — the full command on an
  ordinary card. The boundary is pinned by a test so moving it is a visible
  decision.
* **The descriptor budget is assumed, not measured.** [Bounds](#bounds) derives
  the connection caps from macOS's default soft `RLIMIT_NOFILE` of 256 for a
  launchd job. Nothing reads the actual limit at startup, so a plist that raised
  or lowered it would leave the caps conservative or optimistic; the arithmetic
  is pinned by a test, but the 256 in it is a constant rather than an
  observation.
* **The migration cannot un-splice history that was already spliced.** Facts
  recorded before session uids existed are keyed by name, so a name that held
  two runs becomes *one* migrated run — the log never recorded where one stopped
  and the next began. The guarantee starts from the migration forwards.
* **`codeconnect claude --resume` in a reused name re-reads the transcript.** The new run
  has its own cursor, so it ingests the file into its own log rather than
  finding a cursor that says the bytes are consumed. That costs one backfill and
  is the honest reading: the resumed history genuinely belongs to the new run.
* **`answer` without a `session_id` is resolved by request id.** A client on
  protocol minor 1 cannot say which run it means; the daemon matches the live
  approvals, then the ledger's most recent entry. Two runs holding the same
  `request_id` open at once is refused with an instruction rather than guessed.
* **Sessions started before this build cannot be answered from the phone.**
  Their supervisors report `protocol_minor` 0 and cannot check a prompt
  fingerprint, so approvals are shown but not actuated. `codeconnect claude` again (or
  re-attaching a restarted session) clears it; it is logged once at
  registration.
* **A card is briefly not remotely actuatable after it appears.** The prompt is
  fingerprinted a few hundred milliseconds after the hook fires, because that is
  when Claude draws it. An answer arriving inside that window is refused with a
  reason rather than typed against an unverified screen.
* **A prompt the operator is interacting with reads as a different prompt.**
  Moving the selection with the arrow keys changes the block the fingerprint
  covers, so a phone answer is refused. That is the intended direction: whoever
  is at the keyboard owns that prompt.
* **A `send_text` from a client below minor 3 is not idempotent.** There is
  nothing to recognise a retry by, so a retried takeover types twice. The daemon
  logs it rather than pretending otherwise.
* **The prompt fingerprint assumes the pane is static while a prompt is up.**
  The agent is blocked at that point, so nothing streams — but a future TUI that
  animates inside the prompt block would produce refusals rather than wrong
  keystrokes. Failing that way round is the deliberate choice.
