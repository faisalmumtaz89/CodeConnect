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
codeconnect pair --ssh            # …and let that one pairing install the app's SSH key
codeconnect devices               # paired devices
codeconnect revoke <device>       # revoke a device's token and its SSH key
codeconnect ssh-revoke <device>   # remove only that device's SSH key
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

### A code the phone could not reach is never printed

`ccd` resolves its address **once, at startup**, and every code minted afterwards
carries whatever it resolved. launchd starts the daemon at login and does not wait
for the Tailscale tunnel, so losing that race leaves the daemon bound to loopback —
and it used to print a perfectly scannable QR containing `127.0.0.1`, exit 0, and
add a note saying Tailscale encrypts the link. It does not; there is no tailnet link
in that state. Every visible sign was success and only the code was dead.

Two checks now make that state impossible to hand to a phone:

* A daemon bound to loopback advertises **loopback**, never a MagicDNS name. The
  endpoint and the socket are one claim, and a name that resolves somewhere nothing
  is listening is worse than an address that is visibly local.
* `codeconnect pair` refuses to print a code whose host is loopback, link-local or
  unspecified, and names the recovery: bring Tailscale up, then
  `codeconnect daemon restart`.

The phone applies the same rule to a scanned code, offline, before it dials — see
`PairingQRPayload.unreachableHost`. Both sides reject only what is *definitionally*
unreachable; neither demands a `100.64/10` address or a `.ts.net` name, because a
custom DNS name or a deliberately pinned `ws_bind` are legitimate.

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

**The token is revoked before the SSH key is removed**, and a key that cannot be
removed no longer prevents the revocation. The old order removed the key first
and propagated its error, so an `authorized_keys` that could not be rewritten —
a read-only directory, a full disk — returned a failure *before the token was
ever revoked*: the operator saw an error and the phone kept a working
credential. The two grants are independent. A key that survives is reported as
`ssh_key_removed=false` and logged as `REVOCATION INCOMPLETE` with the line to
delete by hand. (`codeconnect ssh-revoke` is the exception: it withdraws nothing else, so
a failure there *is* the operation failing and is reported as an error.)

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

### `codeconnect pair --ssh`

Only a code minted with `--ssh` lets the app's `ssh_pubkey` be appended to
`~/.ssh/authorized_keys`. The consent is bound to that one five-minute code, not
to the daemon's configuration, and the phone cannot request it — a key offered
against an ordinary code is logged and dropped, and the pairing still succeeds
so the app can fall back to another SSH credential.

Only a bare `ssh-ed25519` entry is accepted: the first field must be the
algorithm, which rejects the whole authorized_keys options grammar
(`command="…"`, `from="…"`), and the base64 is decoded and structurally checked.
Multi-line input is refused outright rather than sanitised. Entries are written
atomically (temp file + `rename`, following a symlink to its target) and tagged
twice — a marker comment and the key's comment field — so `codeconnect ssh-revoke`
removes exactly ours and nothing else.

The temporary carries **128 random bits** and is created with `O_CREAT|O_EXCL`.
The old name was `.authorized_keys.codeconnect.<pid>` — entirely predictable, in
the one directory whose purpose is deciding who may log in. Anything able to
create a file in `~/.ssh` first could plant that name as a symlink and have
CodeConnect write SSH keys through it, or plant a regular file and have it
renamed into place as `authorized_keys`. The random name removes the guess and
`create_new` removes the plant: `O_EXCL` fails outright on an existing path *and*
refuses to follow a symlink at the final component. The open descriptor is then
checked — regular file, one link, owned by us — before a single byte is written,
and the directory is `fsync`ed after the rename so a crash cannot leave `~/.ssh`
with neither the old file nor the new one.

CodeConnect never enables Remote Login or Tailscale SSH. Installing a key is not
the same as turning on a server; if neither is running, the key simply sits
there unused.

## TLS

At startup the daemon reads its MagicDNS name from `tailscale status --json`
and, if `tailscale cert` succeeds, caches the certificate under
`~/.codeconnect/tls/` and serves `wss://`. The certificate is re-issued when it
has fewer than `cert_refresh_days` (30) of validity left — read out of the
certificate's own `notAfter`, not from a sidecar note, so a certificate replaced
by hand is still assessed on its merits.

**Clients must connect by MagicDNS hostname.** The certificate's SAN is a DNS
name; `wss://100.x.y.z:8787` cannot validate against it. That is why the QR
carries the MagicDNS name rather than the tailnet IP, and why the daemon keeps
using that name as the QR host even when it has no certificate.

Both schemes share port 8787. The listener peeks the first byte — a TLS record
always starts `0x16`, an HTTP request never does — so a phone that has not been
updated keeps working. Set `tls_required: true` once every client speaks
`wss://` to refuse plaintext.

If `tailscale cert` fails (commonly: HTTPS Certificates are not enabled for the
tailnet, under *admin console → DNS → HTTPS Certificates*), the daemon logs why,
serves `ws://`, and reports `capabilities.tls: false`. Tailscale still encrypts
the link; what is lost is the certificate, not the confidentiality.

## What the phone can ask for

`hello_ack.capabilities` reports `tls`, `tls_active`, `diff`, `risk_class`,
`session_uid`, `send_text_idempotent`, `prompt_identity`, `push`, `send_text`,
`capture` and `delete_session` so the app disables affordances it does not see
advertised instead of failing at tap time.
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
`delete_session`.

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
| `ws_bind` | tailnet IP | Explicit bind address; otherwise `tailscale ip -4`, else loopback. Bound to loopback, the daemon advertises loopback and `codeconnect pair` refuses to print a code — see [Pairing](#pairing). |
| `ws_loopback` | `true` | Also listen on `127.0.0.1`, for tools on this Mac. The token is still required. |
| `gate_hook` | `"PermissionRequest"` | Which hook waits for the daemon. `"PreToolUse"` or `"none"` also valid. |
| `hold_ms` | `0` | How long to hold the gate hook for a phone answer. `0` = never hold. |
| `unreachable_ask` | `false` | When the daemon is unreachable, make PreToolUse return `ask` with our reason. Renders the reason to the operator, at the cost of prompting on every tool call. |
| `input_box_needles` | built-in | Whitespace-insensitive needles proving the composer is ready. |
| `permission_prompt_needles` | built-in | Needles proving a permission prompt is on screen. |
| `send_keys_delay_ms` | `120` | Pause between typing text and pressing Enter. |
| `tmux_status` | `false` | Show tmux's status bar inside the session. |
| `claude_bin` | auto | Explicit path to the real `claude`. |
| `tls` | `true` | Try for a `tailscale cert` and serve `wss://`. Falls back to `ws://` rather than refusing to start. |
| `tls_required` | `false` | Refuse plaintext. Turn on once every client speaks `wss://`; both share one port until then. |
| `tls_hostname` | auto | Override the MagicDNS name used for the certificate and the QR host. |
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
cargo test                        # 417 tests, including fixture replay
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

Tests that write to `~/.ssh` take `ssh_keys::test_home::FakeHome`, which
serialises and restores `HOME`. `HOME` is process-global while tests run in
parallel threads, so that guard is the only supported way to run one.

## Known limits

* **APNs is a logging stub** until a `.p8` key exists; `hello_ack` advertises
  `push: false` so the phone can tell "not configured" from "failed".
* **TLS depends on the tailnet.** `tailscale cert` needs HTTPS Certificates
  enabled for the tailnet. Without it the daemon serves `ws://` and says so.
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
* **`authorized_keys` edits are serialised inside this daemon only.** Concurrent
  `install`/`remove` cannot lose an update, but an `authorized_keys` being
  edited by a text editor at the same moment is outside what any lock here
  could arbitrate.
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
