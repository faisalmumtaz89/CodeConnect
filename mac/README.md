# CodeConnect — Mac side

Two shipped binaries and a shared library in one cargo workspace.

| Crate | What it is |
|---|---|
| `protocol` | Shared wire types: event envelope, unix-socket frames, WebSocket protocol, session identity, config. |
| `ccd` | The daemon: SQLite event log, agent hosts, tailnet WebSocket server, push. |
| `codeconnect` | The command line: the daemon's LaunchAgent lifecycle, pairing, sessions, the mail tools an agent calls (`mcp`) and updates. |

## Run it

This builds from source. To install the signed release instead, follow
[Install](../README.md#install) in the top-level README.

```sh
./install.sh                      # build + install to ~/.codeconnect/bin
export PATH="$HOME/.codeconnect/bin:$PATH"

codeconnect daemon install                 # start the daemon now and at login, restart it on crash
```

A source build and a release both install the agent host, `cc-agent-host`, beside
the binaries: ccd runs it for every Claude run started from the app. A source build
builds it with `agent-host/build.sh`, which fetches the Bun it pins.

## Codex sessions

A Codex session is started from the app: ccd runs Codex's own JSON-RPC app server for
it, through the login shell, and talks to it over that server's socket.

Both kinds of session get one MCP server of CodeConnect's own, `codeconnect`
(the Agent SDK's `mcpServers` option for Claude Code; `mcp_servers.codeconnect` in the
config of a Codex run's thread), with which an agent lists the other running sessions, sends one a
message, and reads its own. `ccd` keeps a message until the receiving session reads it, on
this computer or, carried over Tailscale to that computer's `ccd`, on another of the same
Tailscale user's computers. See
[docs/codex.md](../docs/codex.md#messages-from-other-agents).

**[`docs/codex.md`](../docs/codex.md) is the operator page.**

What the phone is *allowed* to offer on a Codex session is decided by the daemon's
protocol minor, which `codeconnect daemon status` prints:

```text
daemon   pid NNNNN · version 0.6.0 · build <build id>
         protocol 1.20 · up since 2026-09-09T00:00:00.000Z
```

The phone hides what the daemon has not advertised rather than failing at the tap. Codex
support was built across five of those numbers:

| Minor | What the daemon gained |
|---|---|
| 1.16 | The phone can answer a Codex approval — `AnswerPath::CodexResponse`, which is neither a hook return nor a keystroke but the app server's own response, written for it. |
| 1.17 | Stop is honoured rather than refused. Every `InterruptResult` status becomes reachable, and the `codex_interrupt` capability says this daemon does the thing. |
| 1.18 | Say something — the `codex_compose` message, and a `ComposeResult` that says whether the words *started* a turn or *steered* the one already running. Gated by `codex_compose`. |
| 1.19 | The three facts that make a Codex session drivable from the fleet without inference: `request_id` inside the `approval_resolved` payload, `turn_id` on the approval card's event envelope, and `SessionSummary.codex_link` (`subscribed` · `bound` · `offline` · `none`). `bound` and `offline` are gone in X12. |
| 1.20 | `codex_link` gains a fifth word, `bound_not_started`: a link bound to a thread the daemon has proved has never run a turn. It is the one un-subscribed state a compose is admitted in — no rollout means no turn to collide with, and none to stop — so the phone may start a fresh session's first turn. |

`subscribed` actuates both verbs; `bound_not_started` actuates compose only; `none`
actuates neither. A refusal is still the last word: a link can move between the
summary the phone drew and the tap.

```sh
codeconnect daemon install        # write the LaunchAgent and start it
codeconnect daemon status         # plist, launchd job and live daemon, side by side
codeconnect daemon restart        # launchctl kickstart -k
codeconnect daemon folders        # let ccd open Desktop, Documents and Downloads
codeconnect daemon uninstall      # end its runs, stop it and remove the LaunchAgent

codeconnect pair                  # QR code that pairs a phone (single use, 5 minutes)
codeconnect devices               # paired devices
codeconnect revoke <device>       # revoke a device, and every other on its Tailscale device
codeconnect unblock <device>      # let a revoked device sign in again
codeconnect token                 # the static fallback token
```

## Session identity

`session_uid` is a [ULID](https://github.com/ulid/spec) minted once at spawn and
never reused. It is what the event log and the answers ledger are keyed by, and a
run's `session_id` carries the same value.

Both ride every `event` and every entry in `sessions`. A client on
`protocol_minor >= 2` should send the uid and key its own store by it.


## Card identity

A Claude approval card is answered only through the agent host's return to its
call's `canUseTool` request, never by typing into the prompt on screen — see
[docs/ARCHITECTURE.md](../docs/ARCHITECTURE.md#approving-something-safely).
`ApprovalCard.question_hold` says whether the phone can answer it now, and
`identity_bound` is true exactly on a held approval, for phones that predate the
hold on approvals. A held card lasts as long as Claude waits on it.

## Durability of mutations

An answer or a message from the phone is claimed durably **before** it is handed
to the run's agent host, and the claim is cleared in the same transaction as its
outcome.

That two-phase claim exists for one question a restart could not otherwise
answer: *did it reach the agent?* The host's replay, once the host is attached
again, settles it, and until then it is never sent again. A claim on a run with
no host means the daemon died between the two, and recovery records an explicit
indeterminate outcome — `AnswerOutcome.indeterminate: true` for an approval,
`SendTextResult` status `indeterminate` for a message. It is **never** retried:
a second delivery cannot be taken back, while an unanswered prompt is still
sitting in front of a human.

Open approval cards are persisted too. They used to be memory-only, so a restart
answered "unknown or already-resolved request" to a tap on a card that was still
on the screen. A recovered card is answered again once its run's agent host is
attached again, or, on a Codex run, once Codex asks for it again.

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

`daemon status` shows the running daemon's build the same way.

`codeconnect update` downloads the newest GitHub Release, proves it is signed
by CodeConnect's Apple team, and replaces the two binaries and the agent
host in a single directory exchange — so an interrupted update leaves
the complete old set or the complete new one, never a mixture. Where
CodeConnect.app installed the binaries (it leaves
`~/.codeconnect/bin/.installed-by-app`), the app keeps them current, and
`codeconnect update` refuses with "CodeConnect.app keeps these up to date;
update the app.", followed by the way back: delete that file if
CodeConnect.app was removed.

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
  vouch for is not a place to put one by default. One case falls short of that.
  The iPhone and Mac apps, paired with a computer by name, retry without
  encryption (`ws://`) after two failed encrypted attempts. A computer set up
  this way refuses that retry. But on a network you don't trust, something
  answering in the computer's place could receive what the app sends: its token,
  or the code while pairing. A fix is planned for the next update. A pairing over
  Tailscale is not affected, because the tailnet carries the encryption. A daemon
  with no `ws_bind` set is unaffected — it binds this node's tailnet address,
  where WireGuard has already encrypted the path.
* **SSH is retired.** CodeConnect never installs a key. Taking one back is what
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
  `devices.ssh_fingerprint` and `pairing_codes.allow_ssh`, and on both session
  tables the tmux-era engine's `tmux_session`, `tmux_socket`, `codex_socket` and
  `codex_generation` — named one at a time, never every column this build does
  not recognise. In the credential tables nothing is rebuilt, no table
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
  A refused drop of a session table's column fails the open instead:
  `tmux_session` and `tmux_socket` are `NOT NULL` with no default, so a table
  still carrying one refuses every run this build begins. Nothing is written at
  all when nothing is retired, which is every boot after the first. Outside those
  tables and outside that guarantee, every open removes each run with no engine
  host record — the runs the tmux-era engine made — with everything filed under
  it, drops the tables an earlier build kept for its transcript tailer and its
  deletions, and stamps `user_version` with this build's number. Rolling back
  across the session tables' retirement is one-way: an earlier build's
  statements name the columns this one drops.

  **The push tuple is the one exception to "an added column survives a
  rollback untouched," and deliberately so.** `(push_token, push_environment,
  push_credential)` must move together — the relay checks the credential
  against the token in one request — and a rollback breaks exactly that
  coupling: an older build updates the token in place and leaves a credential
  minted for the token before it, so the way back up would read a pair the
  relay refuses on every push. So `user_version` *is* read at open now, once, to
  tell an upgrade from a restart, and on the upgrade a one-shot pass clears any
  credential whose token pairing cannot be proven current (the phone
  re-registers the whole tuple) and clears the whole tuple off any row a prior
  build revoked without clearing it. It is gated on the version transition
  rather than run every boot precisely because it clears a credential that is
  usually valid: repeating it every start would re-register relay push for ever.
  The column still survives the trip down whole; what the upgrade repairs is the
  *coherence* of the three values, not the presence of the column.

## The LaunchAgent

`codeconnect daemon install` writes `~/Library/LaunchAgents/com.codeconnect.ccd.plist`
with `RunAtLoad`, `KeepAlive{SuccessfulExit: false}` and a five-second
`ThrottleInterval`, then bootstraps it. Measured over three soak runs (15 kills):
`kill -9` on the daemon is recovered in 0.4–4.7s — the spread is
`ThrottleInterval`, which is launchd's floor between restarts and the price of
not letting a crash-looping job spin a core. Every session survives it: its agent
host or Codex app-server keeps running, and the restarted daemon attaches to it
again.

Paths are resolved **at install time**, because launchd hands a process no shell
`PATH` at all. The plist names the ccd binary absolutely and carries a `PATH`
built from wherever `git`, `tailscale` and `claude` actually are on this
machine; the daemon separately resolves those from absolute candidate lists in
code, because the two failures look identical from the outside.

Installing stops whatever ccd is running, its own job included — the plist
cannot be replaced under a live one. A daemon started by hand gets SIGTERM and a
wait for the socket; a managed one gets `launchctl bootout` and the same wait.
Two daemons never share one `~/.codeconnect`: the new one refuses to start while
the old one holds `ccd.lock`, and launchd starts it again a few seconds later. The
wait lets the new one start at once instead. Nothing is lost either way — the
sessions are in their agent hosts and the log is in SQLite.

`--no-takeover` refuses instead of stopping anything; `codeconnect daemon restart` picks
up new binaries without rewriting the plist. A daemon too old to answer
`daemon_info` is identified through `lsof` on its socket rather than guessed at.

`codeconnect daemon uninstall` first asks ccd to end each run it started whose host still
runs, a Claude run's agent host or a Codex run's app-server, as an app's `stop_run` ends
one, since a host outlives ccd by design and would run on with no daemon; it prints how
many it ended. When ccd is not running, it names the hosts still running by pid instead:
an agent host from its `host.json`, a Codex run's app-server from its `as.pid`. It removes the plist only
once `launchctl bootout` has actually succeeded. An unrecognised failure there means the job may still be
loaded, and deleting its definition would leave a daemon that nothing can
manage, restart or stop.

The daemon rotates its own logs (`ccd.out.log`, `ccd.err.log`, capped by
`log_max_bytes`, one `.1` generation kept). It has to be the daemon: launchd
holds those descriptors open, so an external rotator that renames the file would
leave launchd appending to an unlinked inode — the log stops growing, the disk
keeps filling, and nothing says so.

## On Linux

The same two binaries, built static (musl) for x86_64 and aarch64, so one archive runs on any
distribution; checked on Debian 12, Ubuntu 22.04 to 26.04, Fedora 44 and Alpine 3.22. The agent
host beside them, which runs Claude Code for the app, is Bun's glibc build, built and tested on
Ubuntu, so Claude Code runs started from the app need a glibc system. A musl system such as
Alpine does not ship the glibc loader the host names, so the host cannot start there; the two
binaries run there as above.

**Install.** The installer at codeconnect.sh downloads the release's
`codeconnect-<version>-linux-<arch>.tar.gz`, its `.sha256`, and `.sha256.sig` — an Ed25519
signature over that checksum file by CodeConnect's Linux release key — and checks the signature
with `openssl` against the key it pins before the checksum is believed or anything is unpacked or
run. That takes OpenSSL 3 (Debian 12, Ubuntu 22.04 and later, Fedora and current Alpine ship it,
though minimal images leave out the `openssl` command): without it the installer installs nothing and names
the package to add. It never falls back to an unsigned install. `--source` is macOS-only.

**The daemon** runs as a systemd user unit, `~/.config/systemd/user/com.codeconnect.ccd.service`,
under the same commands as the LaunchAgent (`codeconnect daemon install`, `status`, `restart`,
`uninstall`). It restarts five seconds after a crash and stays stopped after a clean exit;
`KillMode=process`, because the agent hosts and helpers ccd starts outlive a daemon restart by design, as
they do under launchd. Its output goes to the same `~/.codeconnect/logs/` files. A user unit stops
at logout unless systemd's *linger* is on for the user, so `install` turns it on with
`loginctl enable-linger` — where the system refuses that to a normal user, it stops before changing
anything and prints the one line to run, `sudo loginctl enable-linger <uid>`. `uninstall` leaves
linger as it is and says so. With no systemd user manager (Alpine and other OpenRC systems, a shell
reached with `sudo -u`, most containers), `install` and `restart` refuse, change nothing, and print
the one line that starts `ccd` by hand, appending to the same log files; `status` still shows the
daemon. Nothing restarts a daemon started that way.

**Updating** works as on macOS, with the release key in place of codesign: `codeconnect update`
and the installer's `install-staged` take the archives named for this machine's architecture, and
believe a checksum only once its signature verifies against `RELEASE_KEY`
(`codeconnect/src/update_install.rs`) — before the digest, the listing, the unpacking, or running
anything. On Linux, `install-staged` installs the release's own verified binaries rather than the
ones the installer handed over. Archives are listed and unpacked with `/bin/tar` (busybox's on
Alpine reads the same).

**Tailscale.** The CLI is looked for at `/usr/bin/tailscale` too, where the distribution packages
put it. An address `tailscale ip` names counts only while this machine can bind it: after
`tailscale down` the CLI still prints the node's addresses while the interface no longer carries
them, so the daemon stays on loopback and moves onto the tailnet when the tunnel is back, instead of
failing to bind on every start. `tailscale up` is refused to a normal user unless they are
Tailscale's operator, so on Linux the installer runs `sudo tailscale up --qr`, and the notes name the
same line: it prints a QR code that you scan with your phone, signed in to Tailscale, and approve
there, so a computer with no screen or browser signs in from the phone.

**The machine's name** on the phone is its hostname — the kernel's, the one `hostname` prints —
read on every handshake, so a rename shows on the next connection.

### Tailscale in userspace mode (containers, cloud sandboxes)

Where Tailscale cannot have a TUN device (`tailscaled` stops with `tstun.New("tailscale0"): operation
not permitted`), it runs with `--tun=userspace-networking`: no interface holds the node's tailnet
address, and tailscaled hands each tailnet connection to that address on to `127.0.0.1`, same port.
The daemon then finds no tailnet address it can bind and asks `tailscale status --json`; when that
says `"TUN": false` and a running backend, it listens on `127.0.0.1:8787` and the pairing code
carries the tailnet IPv4 address, served as `ws://` over WireGuard. Such machines usually have no
systemd either, so `ccd` is started by hand. From a root shell on a fresh Debian or Ubuntu machine:

```sh
apt-get update && apt-get install -y curl openssl
curl -fsSL https://tailscale.com/install.sh | sh
mkdir -p /var/lib/tailscale
nohup tailscaled --tun=userspace-networking --state=/var/lib/tailscale/tailscaled.state >/var/log/tailscaled.log 2>&1 </dev/null &
tailscale up --qr              # scan the QR with your phone, signed in to Tailscale, and approve
useradd -m -s /bin/bash dev    # the installer does not run as root
su - dev
```

Then, as that user, `curl -fsSL https://codeconnect.sh | sh`: with no systemd, its setup starts
`ccd` by hand once Tailscale is connected, shows the pairing code, and prints the line that starts
`ccd` again.

A `ccd` started before `tailscale up` stays on loopback and says in its log when the address
appears; restart it by stopping it (`codeconnect daemon status` shows its pid) and running that
line again. Nothing starts tailscaled or `ccd` again after either stops or the machine restarts.

## Pairing

`codeconnect pair` asks the daemon for a single-use 8-character code (5-minute TTL,
stored hashed) and prints it as a QR, beside the address and code the app's
manual field takes. The QR holds a link:

```text
https://codeconnect.sh/p#v=1&h=<magicdns-name>&p=8787&c=ABCD2345
```

A phone camera opens it in the app when the app is installed, and otherwise on
the page, which offers the App Store. The fields ride in the fragment, which a
browser never sends to the server.

The app connects and sends `hello{pairing_code}` with no token. The daemon
validates and consumes the code, mints a 256-bit per-device token, and returns
it once in `hello_ack{device_token}`; the app keeps it in the Keychain. Only the
token's hash is stored here, so a leaked database cannot be replayed as a
credential.

### Your own Tailscale devices need no code

A `hello` with neither a token nor a code is paired as a code would pair it
(protocol minor 32) when it comes from one of this computer's own Tailscale
user's devices and neither end is tagged: the same rule Mail uses between
computers, read from a fresh `tailscale status` on each such `hello`. The ack
carries a new device token. From anywhere else (another Tailscale user's device,
a tagged one, an address Tailscale does not know, this computer itself) it gets
`unauthorized`, and a code still pairs. Trust is per device, not per person: every
account on a trusted computer, and every app on a trusted phone, is let in.

Each device remembers the Tailscale device it was seen from (its stable node id),
from a token, from a code or from such a `hello`. `codeconnect revoke` revokes
every device with the same node at once, so the phone or computer stays out,
and this computer has no traffic with it either way: its Mail is refused, no
message is sent or retried to it, it is not asked for its agents, and no `peers`
frame names it. `codeconnect unblock <device>` lets it back in: each of its
devices keeps its token, so it signs in again as it was. A device paired before
this release and revoked before it was seen through Tailscale cannot be placed,
so while one is listed every such `hello` is refused; `codeconnect devices` says
so, and unblocking it lifts that.

After its ack, a paired device is sent `peers`: this computer's Tailscale state
and login, and the owner's other computers it can reach (the ones Mail may
message, less the computer the device connects from), each by MagicDNS name
and port. It is read again every minute and sent
when it changes. `fixtures/wire/peers.json` holds the frames this build emits.

### Both ends refuse what is unreachable by definition

`ccd` resolves its address **once, at startup**, and every code minted afterwards
carries whatever it resolved. launchd starts the daemon at login and does not wait
for the Tailscale tunnel, so losing that race leaves the daemon bound to loopback
until its watcher sees a tailnet address and restarts it onto one, a minute or so
later — and with no Tailscale at all it stays there. It used to print a perfectly scannable QR containing `127.0.0.1`, exit 0, and
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
  unspecified, and names the recovery for what chose that bind: with no `ws_bind`,
  set up or connect Tailscale (or set `ws_bind`); with a `ws_bind` naming that
  address, change it; with one naming another address, it is not applied yet —
  then `codeconnect daemon restart` (with no systemd, stopping the daemon and starting
  it again by hand, with the line it prints). The launch note reads the bind the same way.

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
effect *immediately*, for every device on the same Tailscale device. Three
mechanisms, in order of how fast they act:

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
revoking it takes away everything it issued: nothing minted here outlives the
token.
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
the clear. It buys back every capability, and a connection admitted this way is
advertised `private_transport: false`, because "I vouch for this network" and
"these bytes are unreadable on it" are different statements; the phone's pairing
screen shows it as not encrypted. The listener is `OperatorAllowed` rather than
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
`session_uid`, `send_text_idempotent`, `prompt_identity`, `push`, `push_relay`,
`send_text`, `delete_session`, `test_push`, `codex_interrupt`, `claude_interrupt`,
`codex_compose`, `question_card`, `hook_only_approvals`, `history_pages`, `supported_agents`,
`can_approve_reliably`, `command_catalog`, `private_transport`, `start_session`,
`start_worktree`, `browse_folders`, `machine_stats` and `stop_run` so the app disables
affordances it does not see advertised instead of failing at tap time. `push`
and `push_relay` are one-hot: `push` means this Mac holds an Apple key and talks
to Apple itself, `push_relay` means it sends through the CodeConnect relay and a
registration must carry a relay credential as well as a token, and neither means
push is off.
`hello_ack.protocol_minor` is the additive feature level: `>= 1` means
`TurnComplete`, `get_diff`, pairing and `risk_class` are all present; `>= 2`
means every session and event carries a `session_uid`, every message that names
a session accepts one, and `answer` may carry `session_id` to scope itself to a
run; `>= 3` means `send_text` is an idempotent mutation and approval cards carry
a prompt generation; `>= 4` adds the `protocol_mismatch` and `message_too_large`
error codes, the truncated-event placeholder, and two `error` payload shapes an
earlier build's transcript tailer sent; `>= 5` means `lifecycle` is reconciled
rather than merely remembered (an earlier daemon against tmux; this one against
each run's agent host or Codex app-server), so `live` is proven rather than unrefuted, and a
derived `session_end` carries a `reason`; `>= 6` adds `register_push`, the
phone telling the daemon where to send a notification; `>= 7` adds
`delete_session`; `>= 8` adds the slash-command catalog (`command_catalog`);
`>= 9` and `>= 10` added the composer's recovery and the native confirmation after
a typed slash command (gone in X12: nothing is typed into a pane); `>= 11` adds
`SessionSummary.project_label` —
the daemon resolving what a run is *called* (the last component of its working
directory), so that every surface that names a run, including a **direct**
notification the phone cannot compose for itself, uses one string (relay
notifications, introduced at minor 14, deliberately carry no project label and
are titled `CodeConnect`); `>= 13` added the live
terminal (gone in X12; its capability is now `private_transport`); `>= 14` adds
relay-backed push — the `push_relay` capability,
`register_push.relay_credential`, the `credential_invalid` test result, and
`hello_ack.push_environment`, which is the daemon's authoritative APNs
environment for that device's token and the path a relay-side correction takes
back to the phone. `>= 15` opens the agent seam — `AgentKind`,
`Capabilities.supported_agents`, `SessionSummary.agent`, the separate
`CodexResolution` envelope and the composite wire-id codec, with the `interrupt`
operation defined and refused; `>= 16` lets a phone actually answer a Codex
approval; `>= 17` honours `interrupt` and adds `codex_interrupt`; `>= 18` adds
`compose` and `codex_compose`; `>= 19` adds `request_id` to the
`approval_resolved` payload, `turn_id` to the approval card's envelope and
`SessionSummary.codex_link`; `>= 20` adds that field's fifth word,
`bound_not_started`. All additive, and Codex-only:
`codex_interrupt` and `codex_compose` are build facts about the daemon, so a
client scopes both controls by the session's own `agent` and `codex_link` as
well. `>= 21` answers Claude's `AskUserQuestion` from the phone: the
`question_card` capability, `ApprovalCard.question_hold` (`held`; `at_mac`,
`ended` and the `question_hold` events gone in X12), and the `answers` and
`decline` decisions; `allow` and `option` on a question card are refused for
every client.
`>= 22` answers every Claude card only through Claude itself (an earlier daemon
through the call's own hook; this one through the run's agent host), and nothing
is typed: the `hook_only_approvals` capability, `question_hold` on every Claude
card, and `text` on a Claude approval as a denial with that reason. For every
client, any `option` but `1` on an approval is refused, and so is any decision
on a card that is not held. A phone answers Claude approvals only from a daemon that
advertises `hook_only_approvals`.
`>= 23` serves earlier pages and says what a session's latest facts are: the
`history_pages` capability, `history {session_id, before_seq, limit, request_id}`
answered with the newest events below `before_seq` (at most 500) as ordinary
`event` frames and then one `history_end {session_id, request_id, before_seq,
first_seq, error?}`, and on `SessionSummary` the open cards by reference
(`pending`: request id, the seq of its request event, its hold, a Codex card's
turn) and the `model` and `permission_mode` facts, each with the seq of the
event that decided it. A page moves no subscription watermark and is not counted
as a delivery for notifications; a second page asked for a session while one is
being sent is queued behind it. The new summary fields are omitted when empty.
`>= 25` says which computer this is: `hello_ack.machine_name`, the Mac's computer
name or the Linux hostname, read on every handshake (omitted when it cannot be
read), and
`hello_ack.machine_os`, `macos` or `linux`. A phone that gets no name names the
computer by the host it paired with.
`>= 26` says which daemon this is, whatever address reached it:
`hello_ack.machine_id`, minted once into `~/.codeconnect/machine_id` and the same
on every handshake, so a phone that paired this daemon under two addresses (an IP,
then its MagicDNS name) holds it as one machine.
`>= 28` starts a session from the phone, offered (`start_session`) to every
paired device on any connection the daemon admits, and never to the static
token. `list_projects` is answered with
`projects`: every folder a session has started in, most recent first, kept in
its own table so removing a run does not remove its folder, each with its name,
git branch and whether it still exists, and, for the computer, whether each
agent can start as last checked (Claude Code: the check a start makes, below;
Codex: the user's own codex, found from the list the launcher reads, config.json's
`codex_bin`, every `codex` on the login shell's `PATH`, then the install locations,
and its `--version` through that login shell, at least 0.167.0; one not found, below
that or not saying its version is listed unavailable with the sentence that says so,
and a signed-out one with "Codex on <computer> isn’t signed in."): the first list after the daemon starts waits for the
check, and every later one answers from the last check and checks again; when that check finds the agents changed,
`projects` goes out again, unasked, to the device that asked, with the same folders and the agents as they are now. `start_session {request_id, path, agent, prompt?, trust?}` takes a listed
folder (from `>= 30`, any absolute folder) and an optional first message. A start
without one (`prompt` absent, empty or only white space) starts the run waiting
for the person's message: `live`, with no message and no turn, and the first
message the person sends is an ordinary send. `trust` is the person's
answer to "trust this folder?", `trusted` or `not_trusted`, kept for the
folder by its real path (symlinks resolved) in the store's own `folder_trust`
table, so removing a run keeps it. The first start in a folder with no answer
kept, sent without one, is answered `needs_trust {path}` with that real path,
and nothing runs: the app asks, then sends a new start, with a new
`request_id`, carrying the answer. `list_folder_trust {request_id}`,
`set_folder_trust {request_id, path, trust}` and `forget_folder_trust {request_id,
path}` are each answered with `folder_trust {request_id, folders: [{path, trust,
answered_at}]}`, every answer kept, newest first, once
the change is made; a forgotten folder is asked about again. They are served to
the connections `start_session` is. The folder question comes before the agent's
check, so a folder nobody has answered for runs nothing at all.

What the answer does: a Claude Code run started from a folder answered `trusted` loads
the project it runs in, as Claude Code does when you run it there yourself: its
CLAUDE.md, its commands and skills, its MCP servers (as Claude Code approves them) and
its hooks. Hooks are commands the folder's authors wrote, and they run on this
computer, at the run's start and around the agent's tools, without asking you. So
answer `trusted` only for a folder whose authors you trust. A run started from a folder
answered `not_trusted`, or one that runs in a worktree with its own `not_trusted` answer,
starts with your own settings and CodeConnect's own tools only: the agent host passes
claude `--setting-sources=user` and `--strict-mcp-config`. A worktree's own `trusted`
answer never lifts its start folder's `not_trusted`. A run keeps the
answer it started with, a restarted daemon included; changing the answer applies to the
next start.

A Codex run takes the same answer, by the same rules. Answered `trusted`, it loads the
project as Codex does when you run it there yourself: its AGENTS.md, its
`.codex/config.toml` and the MCP servers it names, its rules and its skills. Answered `not_trusted`,
it keeps the project out: every folder is untrusted and the repository's own skills are
off. CodeConnect keeps the answer itself and never writes it to Codex's own configuration.

A Claude Code start
that goes on is then checked, before anything runs: the daemon finds the claude a run would use,
from the `claude_bin` list (below) with
the login shell's `PATH`, and asks that claude itself its `--version` and
`auth status` from the login shell. It is `agent_missing` when no claude is found,
and `launch_failed` when it is older than 2.1.268, does not say its version, or is
signed out (`auth status` exits 1 with `loggedIn` false). `auth status` is read for
`loggedIn` only; nothing else it prints is kept. A Claude Code start then writes
its run (`starting`, under its `request_id`) before anything runs, makes the run's
folder `runs/<uid>/` under the CodeConnect folder (0700), and runs the agent host,
`cc-agent-host <run folder>`, through the user's login shell in a session of its
own, so it outlives the daemon; ccd hands the host the first message as the run's
`start` over the run's link, and the host runs that claude with it, or with no
message when the start has none. The start's
answer waits for the host's outcome: a done start makes the run `live` and is
answered started, and a refused one makes it `failed` and is answered with the
reason; its host then ends, once claude's end is acknowledged. With no outcome within the start's 80 s, the answer is started and the run
stays `starting` until its outcome comes. Once `start` is sent, only that outcome or
its host's end ends a starting run: one whose host lost its link but still runs stays
`starting`, and a delete refuses it; one whose host is gone becomes `failed`. A live run's conversation is what claude sends over the link: each SDK
message becomes the run's events (`sdk` source, through `protocol::sdk_map`), filed with
the run's cursor in one transaction before it is acknowledged, and each init also names
the run's Claude session id and the folder claude runs in, a new worktree's included.
A command's own output (`/usage`'s, `/effort low`'s, `/compact`'s "Compacted") is filed as a
`user_message` whose content is claude's `<local-command-stdout>` string.
It is stored with the run's history, account figures included when the command prints them, as
`/usage` does.
A live run ends when claude does: its `exited` is filed with the run's cursor in one
transaction, the run `exited` and its `SessionEnd` carrying the exit code, any signal, and
the reason "Stopped: its process ended", since nobody stopped it. It
also ends when its host is gone, seen when the run's link ends or by the liveness tick. ccd
then sends SIGTERM to the host's process group, and SIGKILL 5 s later only while members
remain, each under a warrant: the same boot, and the host's pid free or still its own. The
run is then `exited`, "Stopped: its process ended". A ccd restart attaches each Claude run's
host again while it still runs, from the run's cursor: the host replays
what was not acknowledged and the run goes on; a run still starting is sent its start again,
which a host that has one ignores; and the host of a run that has ended is drained, so it
exits. A host gone meanwhile is the liveness tick's, as above. A starting Claude run whose ccd
died before it recorded the run's host becomes `failed`.
An app's message to a live Claude run (`send_text`) goes into claude's input through the run's
host, as one more message from the person; the person's words, the start's prompt
included, are filed once the host has taken them. A later message whose first word is
`/<name>`, `<name>` one of claude's commands or one of their aliases as the run last had
them, is filed naming it as typed (`command`: `/effort low` is `effort`, and `/cost`, an alias
of `usage`, is `cost`, as claude's end of it names it); any other text, and the start's
prompt, names none.
A command's own turn ends naming it too, with claude's own `local_command` on the
`turn_complete`, which an agent turn's end never carries. A message sent while one of Claude's
tools is running reaches Claude with that tool's result; one sent at any other time in a
turn runs as Claude's next turn. A retry with the same request id is answered from its
first send, so nothing is sent twice.
A run whose claude ended without anyone stopping it (its end reads "Stopped: its process
ended", after a crash or a Mac restart alike) is relaunched by the person's next message,
never at ccd's start; its summary says so (`relaunchable`, minor 42). A run End run ended, a
start that failed, and a run claude never named a session for (one started without a first
message whose claude ended before any, with nothing to resume) stay ended ("This session isn't
running, so nothing was sent."). Before anything is launched, the run's old host must be gone
and its process group empty, unless it was recorded before this Mac last started ("This
session is still ending; nothing was sent. Try again in a moment."), and a start's own checks
must pass. The run then reads `starting`, so a second message meanwhile is refused, and a new
host in the run's folder resumes claude's session in the folder claude ran in, with the
message as its first words: the run's model as its menu marks it chosen, the folder answer it
started with (lowered if the folder has since been answered not trusted), and the mode the
run was in when it is "Ask before edits", "Accept edits" or "Plan", so such a run does not gain
permissions by starting again (the owner's decision 253); a run in Bypass permissions, or in a
mode the menu does not list (such as `dontAsk`), is given none, so the new claude starts in the
person's own default, which can be wider than an unlisted mode; a mode that cannot be read
refuses the message, nothing sent. The mode is the one claude last reported: a change claude
took and did not report before it ended is not kept. Its first init says
`source: "resume"`. Done,
the run is `live` and the message `sent`; claude's refusal leaves the run `exited` and still
relaunchable, with claude's reason; a launch that fails first puts it back `exited`, nothing
sent. With no answer within 15 s, inside the apps' 20 s wait, the message reads
indeterminate and the relaunch goes on. The effort chosen for the old process is not carried
over: the run's effort reads unset from its end until one is set again. A ccd restart in the
seconds of a relaunch files the resumed start with no `source`.
For such a run the Model and Effort screens are refused, as a sheet's typed confirmation or
typed alone (`/model`, `/effort`) ("/model's screen isn't available in this session;
nothing was changed.", the same for the effort: the sheets set them with `set_model`), and so is `/status`, which claude does not offer such a run ("Claude
doesn't offer /status in this session."); `/usage`, `/cost` and `/clear` are messages. A
clear goes on in the same run, as at claude's keyboard: claude starts its new session, the
run's events carry a "Conversation cleared" notice with the earlier history above it, and a
later resume continues the cleared conversation. Each init's `session_start` says why a session
starts: `source: "startup"` at a launch's first init,
`"clear"` at the first after a `/clear`, and none at a turn's later init. The run's command menu
(`get_command_catalog`, minor 38) is what its claude last reported: the start's initialize
answer, then each `commands_changed`, without a claude.ai
connector's commands (the host leaves them out of both), the ones the latest init keeps to
claude's own screen, or those two refused screens, each with claude's description and
argument hint (`entries`); it is `unavailable` until an init names the commands claude keeps
to its own screen.
An app changes such a run's model, its effort or its permission mode with `set_model` and
`set_mode` (minor 39), each answered with a `setting_result`. Each value must be one the run's
`menus` offer (its summary's while it is live: the models of its start's answer, each with
the efforts claude lists for it, and four modes, the agent's asking default first: "Ask
before edits", "Accept edits", "Plan" and "Bypass permissions"), checked before anything is
sent. Bypass permissions asks nothing,
so its menu entry is marked `confirm` (minor 40): an app sends it with `confirmed` once the
person has confirmed it in the app, and ccd refuses it without, nothing sent ("Turning on
Bypass permissions needs your confirmation in the app; nothing was changed."). Every hosted
claude starts with the SDK's guard for that mode set (`allowDangerouslySkipPermissions`),
which names no mode: a start takes the person's own `defaultMode`, so a person whose own
settings default to `bypassPermissions` now gets it on a run started from the app without
the app's confirmation, as when they run claude directly, and a project's settings cannot
set it (claude drops an escalating `defaultMode` from repo-committed settings); the run's
mode fact says so from its first turn. A start the person's own settings put in a mode the
menu does not list (such as `dontAsk`) carries that mode as its fact, which no menu row
matches: neither app checks a mode then, the Mac's mode button reads "Mode", and the person
can still choose a listed mode. A card already open when Bypass permissions is
turned on stays open and answerable; the mode applies from claude's next request. A ccd restart keeps the mode the person confirmed for
the same process; a new claude, whether a fresh run or a relaunch of a run in Bypass permissions, starts in the person's own default. Each part then goes to the host as a
line of its own, the model before the effort, and the host makes the SDK's own call for it
(`setModel`, `applyFlagSettings`, `setPermissionMode`), one line at a time. The answer is
`applied` `now`, as claude takes a model or a mode from its next request, the running turn's
next step included, and an effort set while a turn runs `next_turn`, as nothing yet shows when
claude applies one (a model and an effort in one change, which no app sends, is answered by
its effort); `refused` with
claude's reason, saying so when the model was changed and the effort was not; or
`indeterminate` when no answer came within 10 s or the run's link ended first. The model and
the mode then show as claude reports them, its `Set model to …` receipt and its permission-mode
status. The summary's `menus.selected_model` names the value in `models` that model is, for an
app to show as chosen (display only): the model itself when it is a value, else the alias of
claude's receipt (`sonnet` of `sonnet (claude-sonnet-5-5)`), else the one model claude's start's
answer resolves to the model's id; when two do (`default` and `opus` both resolve to
`claude-opus-5-5`), the alias of the run's newest receipt if it is one of them, else `default`.
It is left out when none matches (a typed `/model`'s `Sonnet 5.5`). claude echoes nothing for an app's effort, so that shows from a `model_set` notice ccd
files when claude takes it, even from an answer that comes late; an effort typed as
`/effort low` shows from claude's own `Set effort level to low …` receipt (a receipt in another
wording is not read). A retry applies the change again. A change whose answer is lost
across a ccd restart leaves the effort as it was last set.
From minor 43 a Codex run CodeConnect started takes the same `set_model` and `set_mode`. Its
`menus` are the models Codex lists as the agents' check asks it (`model/list`; the answer a
list kept, or, with none kept that says it is signed in, asked at the start; kept with the run),
with the model the run started on first when that list does not carry it (one the person's
config names that Codex does not list, or an account default Codex's online catalog gained
after its signed-in answer was kept), labelled with its id and offering no effort,
since Codex names none for it; and
four modes, the asking default first, an order that is CodeConnect's and not Codex's: "Default"
(`auto`: approval on request, workspace-write sandbox), "Read Only", "Plan" and "Full Access"
(`full-access`: approval never, no sandbox), marked `confirm` and refused without it as
Bypass permissions is. Each change goes to Codex as one `thread/settings/update` on the run's
link, answered `applied` (`now` with no turn running, `next_turn` while one runs, as Codex
takes a change from its next request), `refused` with Codex's words when it answers with an
error, or `indeterminate` when it does not answer within 10 s. Plan keeps the run's approval
and sandbox, and a mode carries the model and the effort Codex last named, since Codex makes
a mode's own the thread's; a mode change before Codex has named the run's settings is
refused, nothing sent. The run's model, effort and mode are the settings Codex names: in its
answer to the run's start and to each resume, and in its report of each change
(`thread/settings/updated`), since it reports none at a start, a resume or a plain turn. A run
that asks nothing reads `full-access`, in Plan too, and a combination none of the four is gives
no mode. A run whose Codex ccd reconnects to after its own restart is resumed with the
approval and sandbox Codex last named for it, so the same Codex keeps Full Access; a run whose
Codex is started again (relaunched) comes back in Read Only or Plan when it was in one of them,
and is given Default's otherwise, as its start was: a run never gains permissions by restarting,
beyond the settings Codex last reported, and a new Codex is never given Full Access (the
owner's decisions 233 and 253). A resume does not carry Plan, so every resume of a run in Plan,
a relaunch's, a reconnect's and the follow-up after a turn that ran across a reconnect, sets Plan
again before the run takes a message; when
Codex does not take it, a relaunch sends nothing and the person is told, and a reconnected run
takes no message until ccd's next start. A resume that sets Plan again keeps Codex's report of
Plan as the run's settings, not the answer's Default, so a refusal, a crash or a restart before
that report leaves Plan the kept mode and the next resume sets it again; any other resume reads
what its answer names. A settings change Codex takes, an app's or the Plan a resume sets again, is
the thread's settings for the run's link until Codex reports them, so the follow-up resume after a
turn that ran across a reconnect asks for what the person set. Only Codex's reports are filed, and
a reconnect after ccd's own restart and a relaunch read those, so **a change Codex took while a
turn ran and had not yet reported when ccd restarted or Codex was started again is not kept**: the
run comes back with the settings Codex last reported. Measured on Codex 0.170.0, a change to
Plan or Read Only made while a turn ran was reported and filed within 4 ms of the app sending it.
When claude asks permission for a tool call, its host holds the request and ccd shows it as
a card the apps answer: allow, deny, or deny with a reason, and, when claude's request
suggests an allow rule for the call, "Allow for this session" (`allow_for_session`, minor 36;
the app answers `option_id: "acceptForSession"`): claude gets its own allow rules for the call
with the session as their destination, so they last only while this session runs and are never
written to claude's settings; a mode change or a folder claude suggested with them is not sent.
ccd decides that from the card it stored, and refuses the answer on a card that does not offer
it, nothing sent. A command claude's own rule cannot match (one with arithmetic expansion or a
redirect, for example) is asked about again, as when claude runs directly. The answer goes to
claude through the host, and the card closes as answered once the host has written it into
claude's input; a request claude stops waiting on (End run, or claude's end) closes with
nobody having answered. A card stays answerable across a ccd restart, and an answer sent just before one
is never sent twice: the host's replay says whether it reached claude. A request whose card
would be too large for an app to show is refused at once, and claude is told why. A question
claude asks (`AskUserQuestion`) is a card too, answered with the options picked (and any
"Other" text or notes), which go back to claude as the call's input, or declined, which
denies it, "Declined from iPhone" or "Declined from Mac", and stops the main conversation's
turn but not a subagent's; nothing about how the terminal would draw its text keeps it from
the apps, and an allow, an option or a reason on it is refused. Once the host confirms either
answer, the card reads answered or declined. A shell command that only reads raises no card:
claude runs it without asking, as it does when run directly.
An app ends a live Claude run with `stop_run`: the run becomes `stopping` and its host, if its link
is attached, is sent `stop`, which interrupts claude, ends its input and closes the query;
claude's end then ends the run `exited`, "Stopped", with no exit code, since the person asked
for it. A host that has not exited 10 s after the stop, told or not, has its group stopped by
the same ladder, and the run ends "Stopped" too. A run still starting is refused, and one that has ended answers
`already_ended`. A ccd restart sends a stopping run `stop` again and starts its 10 s again. A run CodeConnect
did not start is refused.
An app stops the turn a Claude run CodeConnect started is running with `interrupt`, Codex's
Stop with the same frames and answers (`claude_interrupt`, minor 37), naming the turn by its
events' `turn_id`. ccd sends
the host `interrupt` with that turn, and the host interrupts claude, its queued messages cancelled
(`cancel_queued`), only while that turn is the one claude is running; otherwise it refuses and
nothing is sent. The Stop reads `aborted` when the turn ends interrupted, and "that turn ended on
its own" when it finished first. A second Stop of the turn while one waits, and a Stop of a run
that is ending, has ended or is still starting, are refused at once. A message the Stop cancelled
before claude ran it gets a `send_not_sent` notice ("Not sent: stopped", with its `request_id`,
which the person's own row of it carries too). A card open when the Stop ends the turn closes
"The turn was stopped before anyone answered." A Stop in flight when ccd restarts reads unknown,
and is never sent again.
A Codex start that goes on is checked the same way: the daemon finds the codex a
run would use, from the `codex_bin` list, and asks it its `--version`. It is `agent_missing` when none is found, and
`launch_failed` when it is older than 0.167.0 or does not say its version. It then
writes its run (`starting`, under its `request_id`), makes the run's folder as
above, and runs, through the user's login shell in a session of its own, a
`/bin/sh` line that claims the run (from the run's folder it writes its own pid to
`as.pid`, never over one already there) and becomes `<codex> -c projects=<list>
app-server --listen unix://<run folder>/as.sock`. The list is every folder from the
repository's top down to the start's, by its real path, or that folder alone outside
git, each at the folder answer's level, `trust_level = "trusted"` or `"untrusted"`. The
run's process is that one pid throughout,
recorded with the birth time the kernel gives it. The daemon connects over `as.sock`,
sends `initialize` and `initialized`, and answers started with the folder. The run's
own task then sends the first message on that connection:
- `skills/list` for the folder;
- `thread/start` with the folder, the start's model (none for Codex's own `default`),
  `approvalPolicy: "on-request"`, `sandbox: "workspace-write"` and a thread config:
  - the same folder list, at the same level;
  - for a folder answered `not_trusted`, every repository-scope skill `skills/list`
    reported, disabled;
  - CodeConnect's mail server (`codeconnect mcp`) with the run's uid in its `env`;
- the thread recorded on the run's row;
- `turn/start` with the first message under the start's own uuid as its
  `clientUserMessageId`, when the start has one.

The run is then `live`, and the connection is kept as the run's one link: each event
Codex reports on it (the thread, each message once it is finished, each command or
file change as its call and then its result, the turn's end) is filed once and
reaches both apps. Codex's own requests come on it too. A command, a file change or an
MCP tool call that Codex asks to run (CodeConnect's own `send_message` among them)
becomes a card on both apps. It offers the answers Codex offers for it among accept,
accept for this session, decline and cancel, under the words the apps show: a command's
"Yes, proceed" and "No, and tell Codex what to do differently" (cancel, which ends the
turn); a file change's also "Yes, and don't ask again for these files"; an MCP tool
call's "Allow", "Allow for this session" and "Deny". It never offers Codex's permanent
"don't ask again for commands that start with …", which would write Codex's settings. The answer goes to
Codex, and the card closes once Codex confirms it, or when its item or turn ends. The
questions Codex asks the person in Plan mode become a question card too (`tool_name`
`request_user_input`, each question with Codex's own id): an app answers each question
with one of its options or "Other" and its own words, or declines, and ccd sends Codex
the answers as Codex's own terminal does (an option's label; "Other" as `None of the
above` and the words as a `user_note`; a decline as no answers). An answer sent as ccd
restarts is sent again when Codex asks again. Any other request Codex sends, a question
that does not hold the turn or a form an MCP server asks for among them, is answered at
once with an error, so the turn goes on. Nothing is
written to Codex's own config. A start with no `codeconnect` to run the mail server is
refused before its row. A start that fails once codex is launched (codex ends, does not
listen, or does not take the first message within 80 s, before or after the answer) is
`failed`, and so is its run, whose process group then gets SIGTERM, and SIGKILL 5 s
later only while members remain, under the same warrant. A `live` run whose app-server
dies ends `exited`, "Stopped: its process ended", as a Claude run does.
The person's next message relaunches such a run (it crashed, or the Mac restarted), never
ccd's own start; a run ended by End run, and a start that failed, are not relaunched. The
message moves the run to `starting` and clears its recorded app-server in one write, so a
second message meanwhile is refused and only one relaunch runs. Once the old app-server's
process group has no member (after a Mac restart the group is not asked: nothing of it survived,
and another process may hold its number now), ccd removes the run's `as.pid` and `as.sock`,
launches the codex
a start would find, with the environment a start reads and the run's own folder answer, into
the run's folder, its output added to the run's `host.log`, and resumes the run's thread on
that connection, in Read Only or Plan when the run was in one of them and at Default's approval
and sandbox otherwise. The run is then `live`, its timeline gets a
`session_start` under the new launch ("Session resumed"), and the message is sent on the new
link before any other. A relaunch that fails before that stops what it launched, puts the
run back to `exited` and relaunchable, and refuses the message with "CodeConnect has no
connection to this run right now, so nothing was sent." That holds when ccd's own watch found
the new app-server gone first and failed the run for it; a ccd that dies during the relaunch
is a known limit below. Each end of a hosted run is filed under its launch
(`exit:<uid>:<leader pid>`), so a relaunched run's next end is recorded too, and an end ccd
found for a launch the run no longer has, an app-server a relaunch has replaced, changes
nothing. A relaunched app-server numbers its requests from 0 again, so a card's id names the
launch that asked (its leader's start) and never an earlier launch's card.
An app ends a live Codex run with `stop_run` as it ends a Claude run: the run becomes
`stopping`, and its link interrupts the turn Codex is running and sends the app-server's
process group SIGTERM, so Codex finishes and exits, and the run ends `exited`, "Stopped".
An app-server that has not exited 10 s later gets the same ladder (a second SIGTERM, then
SIGKILL 5 s later while members remain), and a ccd restart starts that 10 s again for a run
still stopping. Codex runs each command in a process group of its own, which outlives the
app-server, so the run's link records those groups: when it attaches, every second while a
turn runs, at each turn's end, and before End run's SIGTERM. Once ccd sees that the
app-server has exited, whether by End run or by dying, it sends SIGKILL to each recorded
group that still has members, under the warrant every other group signal uses (the same
boot, and the group's leader either gone or still the process recorded).
An app's Stop (`interrupt`) stops the turn it names through the same link,
and is answered when that turn ends: `aborted` with the turn when Codex ended it
interrupted, or, if the turn finished on its own first, a sentence saying so. A card the
turn was waiting on closes. A retry with the same request id is answered from the first,
and a Stop for a turn whose stop is already on its way is refused at once.
Both apps offer Stop and the composer on the run's `codex_link` in the fleet. For a run
started on the engine it reads `subscribed` while the run's link is attached and the run is
`live`; `bound_not_started` once its app-server died without ccd stopping it, so the apps
offer the composer, whose message relaunches the run, and not Stop; and `none` otherwise:
before its reconnect, after one that failed, while it is stopping, and once End run has ended
it. A run started from an app, of either agent, files a `link_state` fact when its link
attaches (`attached`, "agent host linked"), once the link is in place, and when its link ends
while the run is live (`detached`, "agent host disconnected"); End run and a ccd restart
file no `detached` fact.
An app's message (`compose`) goes through the same link as `turn/start`, under a uuid named
from the run and the request id, as a Claude run's message is. Codex runs it as a new turn
when the run is idle, and adds it to the running turn otherwise. The answer names that turn
(`started` or `steered`), and the person's row of the message, with its `request_id` and
that turn's `turn_id`, is filed then, so every app learns the turn is running, and can stop
it, before Codex records any item of it; Codex's own record of the same words is not filed
again. Codex reads a message added to a running turn at the turn's next step. If the turn is
stopped first, Codex drops
the message without saying so, and ccd files a `send_not_sent` notice for it ("Not sent:
stopped", with its `request_id`). The evidence is that Codex recorded no message under its
uuid before the turn ended interrupted, which is the evidence Codex's own client hands a
message back on. A retry with the same request id is answered from the first, and a retry of
a stopped message is told it was not sent and is not sent again. While a run's
link is not attached, before its reconnect or after one that failed, an answer, a Stop and
a message are refused with "CodeConnect has no connection to this run right now, so nothing
was sent." A retry of one already sent and not yet settled is answered from what was
recorded instead: a Stop or a message is told it was sent and what became of it is not
known, and an answer that it is already waiting to be confirmed. A run that a
restarted daemon finds still `starting` is `failed`, whatever became of its start, before
the daemon listens, and its recorded process group gets the same ladder under the same
warrant. Nothing is sent again, so a first message Codex had already taken is lost with
the run, and the person starts again. A `live` run whose app-server is still running is
connected again: its thread is resumed with the approval and sandbox Codex last named for it
and the same thread config. A thread that never ran a turn has no rollout, which Codex
answers a resume of with "no rollout found"; when the run has had no message, here or on a
relaunch, ccd starts a new thread with the same settings and thread config and records it
as the run's, put back in Plan when the run was in Plan. Any other failed resume is as
before. The turns
it ran meanwhile are filed from the answer, and a turn still running then is read once more when it ends, for the items
it finished while no daemon listened. A card still open stays open. If Codex asks for it
again, the card answers that request; an answer that was on its way when ccd stopped is
sent once more then, and if Codex already had it, the card closes as answered when its
item or turn ends.
Both use `$SHELL -lic` for zsh and bash, `/bin/sh -lc`
otherwise, every value an argument of its own, and the check and the run read the
same environment. The shell starts from the
user's session environment, not the daemon's service one: on macOS launchd's,
with its `PATH` (`launchctl setenv`'s, or launchd's default) and `XPC_SERVICE_NAME=0`,
`XPC_FLAGS=0x0`; on Linux the systemd user manager's, read whole. Either way the
daemon's own location (`CODECONNECT_HOME`) goes with it, and so does its
`TMUX_TMPDIR`, so that an agent in a test rig cannot reach the owner's tmux server;
`CODECONNECT_LOG` and `CODECONNECT_SERVICE` do not. A daemon
started by hand has the session's environment already and uses its own; so
does one whose session cannot be read (`launchctl` or `busctl` failing). When nothing in
it names an encoding (`LC_ALL`, `LC_CTYPE`, `LANG`), `LC_CTYPE` is the platform's
UTF-8 (`UTF-8`, or `C.UTF-8` on Linux), as a UTF-8 terminal sets it; the user's
own startup files run after and win. A Claude Code start is listed from the moment
its row is written, and is answered from its host's outcome, as above; a start that
fails before its host has the start is `failed`, and so is its row. A Codex start
is listed from the moment its row is written too. The same
`request_id` again, on any connection, gets the first answer and starts
nothing, and after the daemon restarts it is found again from its run's row.
`>= 29` lets that start pick a branch or worktree (`start_worktree`), and never
switches the folder's own branch. `get_project_git {path}` reads a listed
folder's (from `>= 30`, any absolute folder's) repository, without writing, fetching or running a hook:
`git worktree list` (the main checkout first, each with its branch, whether it
has uncommitted changes, and whether its folder is gone) and the local branches checked out nowhere, newest first, at most
100. `start_session`'s `worktree` is one of three. `existing` starts in a
worktree git lists. `branch` runs `git worktree add -- <main>/.claude/worktrees/<branch>
<branch>`, with each `/` written `+`, which is Claude Code's own layout. `new`
takes a name by Claude Code's `--worktree` rule: Claude gets `--worktree=<name>`
and makes the worktree itself, and Codex, whose `--worktree` cannot run hosted,
gets one in the same place on a branch named as typed (`git worktree add -b
<name>`), from the folder's current commit. Not Claude's `worktree-` prefix:
a later `claude --worktree <name>` that reopens a branch so named deletes it on a
clean exit, commits and all. `git worktree add` runs in the user's login shell, as
the agent does. `started` carries the folder the session started in, `cwd`, except for
a new Claude worktree, whose folder the fleet shows once Claude is in it.
CodeConnect never removes a worktree it made.
`>= 30` lets the phone browse the computer's folders, as a terminal can `cd`
anywhere (`browse_folders`, offered where `start_session` is). `list_folder
{request_id, path?, show_hidden?}` answers `folder_listing` with the folder's
real path (symlinks resolved, as Finder shows it; the home folder when no path is
given) and the names of its folders and symlinks to folders, in byte order, at
most 2,000 (`truncated` says there were more), leaving out names that start with
`.` unless `show_hidden`, and names that are not UTF-8; or with `error`, in the
words `ls` prints (`Permission denied`). `create_folder {request_id, parent,
name}` is one `mkdir`: a name holding `/` is refused, and every other refusal is
`mkdir`'s own (`File exists`). `start_session` and `get_project_git` take any
absolute folder; a relative path is refused, and a folder becomes a project
when a session starts in it; a start in a folder this daemon is not permitted to
reach (EACCES, EPERM) is refused in the system's words, and any other folder it
cannot open is called missing, as before. On macOS, Desktop, Documents
and Downloads are read with ccd's own permission, asked for once while someone is
at the Mac: `codeconnect daemon folders`, which the codeconnect.sh installer
(1.7.7 or later) runs once the daemon is up, at a terminal and not over SSH,
has ccd itself read each one, so a dialog names ccd and the answer is kept for
its path, which updates keep.
Only CodeConnect's own launchd job answers either. An `Operation not permitted` in
those folders adds where to change the answer (System Settings → Privacy &
Security → Files and Folders → ccd). External drives are out of scope: the first
visit to one can still raise a dialog on the Mac.
`>= 31` adds each session's `git_branch` and `diff_stat {added, removed}` to
`SessionSummary`: the branch checked out in its folder and the lines added and
removed in tracked files against `HEAD` (`git diff --numstat`, run with the
same switches as `get_diff`, so no external diff, textconv or fsmonitor program
runs; a clean filter configured for a file, such as Git LFS's, does, as it does
for `get_diff`; a binary file and a file git does not track add nothing). They are read when a turn ends
and when the daemon starts, never on a `sessions` request, and
omitted outside a repository, before the first read, and on a detached head
(the branch). It also answers `get_machine_stats` (the `machine_stats`
capability, offered to every connection) with one `machine_stats {cpu?,
memory_used?, memory_total?, disk_used?, disk_total?, captured_at}`, taken when
asked and never stored: `cpu` is the share of all cores busy over half a
second, 0 to 1; memory is in bytes, used being total less what is available for
new work (Activity Monitor's Memory Used on macOS, `MemTotal - MemAvailable` on
Linux); disk is in bytes for the volume holding the daemon user's home folder,
used being its size less what that user can still write. A value that cannot be
read is omitted. The sample is answered off the connection's loop, so nothing
else on it waits for the half second.
`>= 33` lets a start pick the agent's model. Each agent in `projects` carries
`models`, `{value, label, detail?}` in the agent's own order and words, and
`signed_in`, to show: a signed-out Claude Code's start is refused, and a Codex
start does not depend on it. The daemon asks the agent itself from the home folder,
once `--version` says it is there: Claude Code as `<claude> -p --input-format stream-json
--output-format stream-json --verbose --no-session-persistence
--strict-mcp-config --settings '{"disableAllHooks":true}'`, which answers one
`initialize` with its models, its own `default` first with the model that is now;
Codex as `<codex> app-server`, the codex that check found, asked for `model/list`
(hidden models left out) and, unless `codex login status` says it is signed in,
`account/read`, and ended once it has answered, since a signed-in app-server
answers `model/list` but never `account/read` and keeps running; it reaches
chatgpt.com as it starts, and signed out github.com too. Claude Code is signed in when `auth status` exits 0
with `loggedIn` true, signed out when it exits 1 with it false, and otherwise not
known; Codex is signed in when `codex login status` says so, or else when
`account/read` names an account or no OpenAI sign-in is needed. An agent keeps its own books as it starts: Claude Code
refreshes its caches in `~/.claude.json`, and Codex begins a plugin sync that the
ask cuts short, leaving a 4 KB `~/.codex/.tmp/git-*` it never removes. So an
answer that says signed in is kept until the agent's `--version` changes or the
daemon restarts. A Claude Code that `auth status` says is signed out is not asked
at all, since its start is refused. One whose sign-in is not known is asked at
every list. A signed-out Codex's answer is asked again only once `codex login
status`, which starts nothing, says it is signed in; that status does not count a
model provider of the user's own, so one set up while the daemon runs is offered
once Codex is updated or the daemon restarts. A Codex start asks the same way when
no answer kept for its version says it is signed in (no list since the daemon started, or one
from before its sign-in), so its run's settings never depend on whether a list came first.
An agent that does not answer within the check's 10 s offers no models. `start_session`'s `model` is given to Claude
Code's host as the run's model; absent, or Claude Code's own `default`, it gets none
and starts with the model its settings name. A Codex run's goes on its `thread/start`,
and Codex's own `default` as none, the same way.
See `protocol/src/lib.rs` for the authoritative ledger.

**`delete_session` is the only verb that removes a run's record.** It names the run
by `session_uid` and never by `session_id`. The
daemon refuses anything whose lifecycle has
not ended (`exited`, or `failed` for an app start that never started), and
anything it still holds an open approval for.
A refusal is a `delete_session_result`, not an error. It deletes the daemon's own record only:
Claude Code's transcripts live under `~/.claude/projects` and are untouched, so
`claude --resume <id>` still works afterwards.

**`protocol_version` is now enforced.** A client on a different *major* is
refused with `error{code: "protocol_mismatch"}` and disconnected, before its
credential is even looked at. It used to be logged as a warning and then handed
the full event log and the approval path — a peer that by the definition of the
number has a different idea of what these messages mean. The major is 2, and the
refusal's message is `protocol::PROTOCOL_MISMATCH`; the apps show their own
sentence for that code. Checking before the token also means an incompatible peer cannot use the handshake
to probe credentials.

* **`send_text{session_id, text, request_id?, payload_hash?, submit}`** is a
  durable idempotent mutation from minor 3. `payload_hash` is
  `sha256` over length-prefixed `(session_id, submit, text)` — the target and
  the submit flag are in it, so a captured frame cannot be replayed with new
  text under an id the ledger already trusts. A retry with the same pair replays
  (`status: "duplicate"`); a retry with the same id and different material is
  refused. The body is capped at 8KB. A request missing either new field is
  refused, and nothing is sent.
  **`require` is no longer a field.** A client that still sends it is unaffected,
  as an unknown key is ignored, not refused.
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
* **`EventKind::TurnComplete`** marks the end of a turn. For Claude it is filed
  from the SDK's `result`, which the run's agent host relays as each turn ends.
  `SessionEnd` now means only what it says: the run ended.
* **`risk_class`** rides on every `approval_request` card as
  `risk{class, matched_pattern}`. `high` for destructive shell patterns, `low`
  for pure read tools, `medium` for everything else — including anything
  unrecognised, because "we do not know this tool" is not evidence of safety.
  Matching is token-based, so `confirm -rf` is not `rm -rf` and `2>/dev/null` is
  not writing to a device node. It is a rendering hint for a human, never a gate.
* **`ResolvedBy::Timeout`** — closed with nobody's answer: any open card of a
  run that ended (detail "the session ended before anyone answered").
* **`AnswerOutcome.indeterminate`** — the decision is known and whether it
  reached the agent is not: the daemon stopped between claiming it and
  recording what became of it. Never retried.
* **`AnswerOutcome.answered_on`** (minor 34) — `iphone` or `mac` on an answer
  from a paired device: the Mac app's when its connection said hello as
  "CodeConnect Mac", an iPhone's otherwise. Absent on every other outcome, from
  an older daemon, and on an answer a restart settled. It chooses words only.
* **`CodexResolution.answered_on`** (minor 41) — the same on a Codex card's
  `answered` resolution by a paired device (`by: phone`). Absent on every other
  resolution, from an older daemon, and on an answer a restart settled.
* **`ApprovalCard.allow_for_session`** (minor 36) — `true` on a card from a run
  started in an app whose request suggests an allow rule: the card offers "Allow for
  this session", answered `option_id: "acceptForSession"`. Left out when false, so an
  older app reads every card as before.
* **`ApprovalCard.question_hold` / `identity_bound`** — see
  [Card identity](#card-identity). Before minor 22, `identity_bound` meant the
  prompt on screen had been fingerprinted for typing, announced by an
  `approval_prompt_bound` event this daemon no longer sends.

## How it fits together

```
iPhone, Mac app ──wss:// tailnet (ws:// fallback)──► ccd ──► SQLite (source of truth)
                                                      │
                     per run, through the login shell, in a session of its own:
                                                      │
                 ┌────────────────────────────────────┴─────────────────────────┐
     cc-agent-host <run folder>                        codex app-server --listen unix://…/as.sock
     ◄── host.sock ──► ccd                             ◄── as.sock ──► ccd
                 │                                                               │
     claude, through the Agent SDK                                         codex's thread
```

`ccd` is never an agent's parent: each run's agent host or app-server starts in a
session of its own and outlives the daemon, and a restarted `ccd` attaches each
one still running again, so `kill -9 ccd` costs a reconnect and nothing else.

One `ccd` per `~/.codeconnect`. Before it opens the database, the token or the
socket, the daemon takes an exclusive `flock(2)` on `~/.codeconnect/ccd.lock`;
a second `ccd` started in the same home exits at once with `another ccd is
already running in …` and leaves the running daemon, its socket and its database
untouched. The kernel drops the lock with the process, `kill -9` included, so
the next start takes the home straight away and removes the stale socket.

## Configuration — `~/.codeconnect/config.json`

Every field is optional. The defaults are what the daemon is validated against.
A key CodeConnect has no setting for — a misspelled one, say — or a `ws_bind`
that is not an IP address is ignored, and every command that reads the file says so — as it does for a file
that is not valid JSON, whose every setting falls back to its default.

| Key | Default | Meaning |
|---|---|---|
| `ws_port` | `8787` | WebSocket port. |
| `ws_bind` | tailnet IP | Explicit bind address; otherwise `tailscale ip -4`, else loopback. The QR host is never one known to point somewhere else: bound to loopback the daemon advertises loopback and `codeconnect pair` refuses to print a code, and bound off the tailnet it drops a name this Mac's resolver puts at another address — see [Pairing](#pairing) and [TLS](#tls). |
| `ws_loopback` | `true` | Also listen on `127.0.0.1`, for tools on this Mac. The token is still required. |
| `claude_bin` | auto | Explicit path to the `claude` to run. Unset, it is the first `claude` on `PATH` as your shell finds it, then `~/.local/bin`, `~/.claude/local`, `/opt/homebrew/bin` and `/usr/local/bin`. A shell alias is not seen: if `claude` is an alias, set this to what it runs. A start checks the claude this list finds. |
| `codex_bin` | auto | Explicit path to the `codex` to run. Unset, it is the first `codex` on `PATH` as your shell finds it, however it was installed, then `~/.local/bin`, `/opt/homebrew/bin` and `/usr/local/bin`. A shell alias is not seen: if `codex` is an alias, set this to what it runs. |
| `agent_host_bin` | beside ccd | Explicit path to `cc-agent-host`, the agent host ccd runs for each Claude run started from the app. Unset, it is the `cc-agent-host` next to ccd's own executable, where every install puts it. |
| `tls` | `true` | Find a certificate for the QR host — cached, dropped in `~/.codeconnect/tls/` by hand, or from `tailscale cert` — and serve `wss://`. Falls back to `ws://` rather than refusing to start. |
| `tls_required` | `false` | Refuse plaintext. Turn on once every client speaks `wss://`; both share one port until then. |
| `ws_allow_plaintext` | `false` | Serve `ws://` on a `ws_bind` whose privacy only you can vouch for. Honoured on a local-network address; refused on `0.0.0.0`, `::` and public addresses. `tls_required` wins over it. See [TLS](#tls). |
| `tls_hostname` | auto | Override the MagicDNS name used for the certificate and the QR host. Dropped where this Mac's resolver puts the name at an address other than the bind; a resolver that says nothing is not a refusal, and the certificate settles that case. See [TLS](#tls). |
| `cert_refresh_days` | `30` | Re-issue when the certificate has fewer days of validity left. |
| `pairing_ttl_secs` | `300` | Pairing code lifetime. Clamped to 30…3600. |
| `diff_max_bytes` | `524288` | `get_diff` cap; larger diffs come back with `truncated: true`. |
| `diff_timeout_ms` | `10000` | Per-`git`-subcommand ceiling. |
| `tailscale_bin` | auto | Explicit path to `tailscale`. Auto is the first of `/usr/local/bin/tailscale`, `/opt/homebrew/bin/tailscale`, the Tailscale app's own (`/Applications/Tailscale.app/Contents/MacOS/Tailscale`) and `/usr/bin/tailscale` that exists. Set, it is the only one the daemon and `codeconnect` run: a path that does not exist means no Tailscale, as if it were not installed (no tailnet addresses, peers or mail between computers, and no certificate issued; a cached one still serves). A test or rig daemon sets one so it never reads this computer's tailnet. |
| `git_bin` | auto | Explicit path to `git` (launchd has no shell PATH). Auto is `/usr/bin/git` when Apple's developer tools are installed, else Homebrew's or `/usr/local/bin/git`; with none, branches and changes are absent. |
| `log_max_bytes` | `8388608` | Cap on each launchd log; one `.1` generation is kept, so the ceiling is twice this per stream. |
| `log_rotate_secs` | `300` | How often the cap is checked. |
| `ws_max_connections` | `64` | Live tailnet WebSocket connections. See [Bounds](#bounds). |
| `ws_max_per_peer` | `32` | How many of those one remote address may hold. |
| `ipc_max_connections` | `4096` | Live connections on the local unix socket. |
| `ipc_write_queue` | `1024` | Frames one IPC connection may have queued for writing before the read loop feels backpressure. |
| `pairing_max_attempts` | `20` | Failed pairing attempts allowed per window. `0` disables the limiter. |
| `pairing_window_secs` | `300` | The window those attempts are counted over. |

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

**File descriptors.** macOS gives a launchd job a soft `RLIMIT_NOFILE` of 256
under an unlimited hard limit, and a soft limit is a fleet size: every live
session holds a socket to ccd for its agent host or Codex app-server. So ccd
raises its soft limit to 10,240 (macOS's `OPEN_MAX`, or the hard limit if lower)
before it opens anything, and every process it starts inherits it. ccd logs the
limit it got. On Linux a systemd user service and an
ssh login both allow far more than 10,240.

The daemon needs a few dozen descriptors for itself — five SQLite connections
plus their WAL and SHM handles, the listeners, the log files. `ws_max_connections`
(64, on each of the tailnet and loopback listeners) and `ipc_max_connections`
(4,096) fit inside the raised limit with room to
spare. The split is deliberately lopsided: the local socket carries the CLI and
every run's mail server, and a mail server that cannot reach the daemon is an
agent that cannot send or read its mail, so the *remote* side gets the smaller
share. A test
asserts the arithmetic, so raising one of these is a decision rather than an
accident. Where the limit cannot be raised, a connection past it fails to be
accepted and the accept loop backs off and tries again.

**Per peer.** The global cap alone would let one misbehaving client refuse every
other device, so no single remote address may hold more than `ws_max_per_peer`.
It is half the global cap, which guarantees room for somebody else. Generous
rather than tight for two reasons: every connection on the loopback listener
shares one peer address, so a small share would act as a global cap on all local
tooling; and a client may legitimately open several sockets at once.

**Queues.** Each IPC connection's writer is fed by a bounded channel. A client
that stops reading — a client suspended with ctrl-Z, a mail server whose process
is stopped — used to have the daemon buffer frames on its behalf without limit.
Now the read loop simply stops taking new work from a peer that is not consuming
its replies.

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
cargo test                        # the workspace suite
```

On Linux, run the suite with `TMPDIR` set to a folder you own: the tests make
their temporary folder owner-only, as macOS's per-user one already is, and the
shared `/tmp` is not yours to change.

## Known limits

* **On a Codex run started from an app, only a question Codex asks in Plan mode is
  answered from an app, and only with one of its options or "Other".** Notes on a chosen
  option, which Codex's own terminal also takes, are not offered; "Other" carries the
  person's words. A question outside Plan mode (Codex's under-development
  `default_mode_request_user_input`) does not hold the turn and is not shown: Codex is told
  nobody answered, so it gets an empty answer and carries on. A form, which only a person's
  own MCP server asks for, is not shown either and is declined. With Codex's
  `tool_call_mcp_elicitation` turned off, an MCP tool's approval is asked as a question
  that is not shown, and the call is cancelled.
* **On a Codex run started from an app, a file change Codex first asks for while ccd is
  restarting is declined.** Codex asks before a file change only when it writes outside
  the folders the run may write, or into `.git`, `.codex` or `.agents`. If it asks in the
  seconds ccd takes to restart, ccd cannot show what the change is, so it tells Codex it
  could not read the request: Codex does not make the change, and no card appears. A
  command asked then does get a card.
* **On a Codex run started from an app, a command ccd did not record can go on running
  after Codex's app-server has gone.** ccd ends the commands it recorded (see End run
  above). It misses one started since its last check that the app-server's death cuts off,
  one that leaves its process group itself, and every command of an app-server that dies
  while ccd has no link to the run: while ccd restarts, or after a reconnect that failed.
* **On a Codex run started from an app, a trusted folder's own approval policy and sandbox
  mode do not apply.** The run starts in CodeConnect's Default, `on-request` with a
  `workspace-write` sandbox, whatever the folder's `.codex/config.toml` says. That folder's
  own workspace-write settings, network access and writable roots do apply, as when Codex
  runs there directly.
* **On a Codex run started from an app, a trusted folder's hooks run only once you have
  approved them in Codex itself.** CodeConnect does not approve them.
* **On a Codex run started from an app, End run can show "stopping" for up to a minute
  after Codex has gone, when ccd has no connection to the run.** That happens when ccd
  restarts while the run is stopping, or when End run follows a reconnect that failed.
  Codex is still ended by force, and the run reads ended at ccd's next check of its runs,
  every 60 s by default.
* **On a Codex run started from an app, a ccd restart that lands just as Codex closes the
  run's thread leaves the run without a link.** Codex closes a thread a minute after
  nothing is watching it. A reconnect made while it is closing is refused, and ccd does not
  try again, so the run stays live with no connection: an answer, a Stop and a message are
  refused, and a mail wake already on its way stays outstanding, so new mail does not wake
  the run. It has been seen only with Codex's closing delay set to two seconds.
* **On a Codex run started from an app, every one of CodeConnect's mail tools asks
  first.** `list_agents` and `read_messages` show a card like `send_message` until they
  are marked as only reading.
* **On a Codex run started from an app, a command that finished while ccd restarted during a
  turn is filed after that turn's reply.** ccd attaches again while the turn runs, files the
  reply and the turn's end as they arrive, and reads the command back once the turn has
  ended, so the command shows below the reply. Nothing is lost.
* **On a Codex run started from an app, a plan Codex proposes while ccd is not attached is
  not shown.** If ccd restarts in a Plan turn's last step, Codex finishes the plan before ccd
  attaches again, and ccd does not read a plan back from Codex's history, so the turn shows
  its question, its answer and its end without the plan.
* **On a Codex run started from an app, a message whose turn's end ccd never sees keeps
  its last state.** If the connection to Codex ends, or ccd restarts, while a message added
  to a running turn waits for Codex to read it, ccd records that what became of it is not
  known. It is not sent again, and no notice says it was not sent, even if a Stop dropped it.
* **On a Codex run started from an app, a message taken just as a turn finishes waits for
  the next work.** Codex records it as the turn ends, with no model step after it, so it is
  answered only when the run next works, for example on the next message.
* **On a Codex run started from an app, the composer and Stop can take up to about 15 s to
  appear after the run starts, and again after ccd restarts.** The apps learn that the run is
  ready from the fleet, which ccd sends only when asked and the apps ask for every 15 s. Until
  then they say there is no live link to the session.
* **On a Codex run started from an app, a message that relaunches the run can outlast the
  app's wait for its answer on a slow install.** The app waits 20 s for a message's answer. A
  relaunch first finds codex and asks its version through the login shell, as a start does,
  then launches Codex and resumes the thread. That took under a second on a test Mac, but each
  login-shell step may take up to 10 s. Past 20 s the app says the message got no answer; the
  relaunch still finishes and sends the message once, and its turn shows in the run.
* **A message ccd took but could not hand to Claude before it stopped is not
  confirmed.** If ccd stops in the instant between recording a message to a session
  started from an app and passing it to that session's agent host, the app says the
  message was not confirmed: it may or may not have reached Claude. Sending the same
  words again, unchanged or with only spaces tidied, is a retry of that message and is
  answered from it, so it is never sent twice. Any real edit, or clearing the box, ends
  the retry: the same words typed again later are a new message. One case is not
  guarded: a reply typed character for character the same as a diff comment just left
  unconfirmed on the same session, where the comment's retry could type it twice.
* **A Stop at the instant a turn ends can stop the next one.** If a turn has just ended inside
  Claude and a message sent at that moment starts the next one, a Stop of the first reaches the
  second: that turn is interrupted, or its message is cancelled before it runs, and the Stop reads
  that the turn ended on its own. Claude's interrupt names no turn, so this cannot be told apart;
  nothing the person asked to stop goes on running.
* **A permission request too large to show is refused.** In a session started from an
  app, a request whose details are too large for the app to show (a very large file
  write, for example) is refused at once rather than approved unseen. Claude is told
  why and the session goes on. A call in the main conversation shows as refused in the
  timeline; a subagent's calls are not shown there, so its refusal is not either.
* **An approval tapped in the first moment after ccd restarts may not stick.** While
  ccd catches up with a session started from an app, which takes a fraction of a
  second, an answer to one of its approval or question cards may reach Claude
  without being recorded. The app then says the answer could not be confirmed and
  the card stays open until the session ends. In that same moment, denying a
  subagent's request, or declining its question, also stops Claude's main reply.
  Nothing is ever allowed that the person did not allow.
* **Which transport carries a push is decided at boot, from the config file
  alone.** `push_enabled: false` is off. Any one of `apns_key_path`,
  `apns_key_id`, `apns_team_id` or `apns_topic` is a request for direct
  delivery, and then all four are required and the key must load — a partial or
  unreadable set is off with an error, never a silent move to the relay. None of
  the four is the ordinary case and selects the CodeConnect relay, which holds
  the key that cannot be shipped to a customer's Mac. `hello_ack` advertises
  `push` or `push_relay` accordingly, so the phone can tell "not configured"
  from "failed" and direct from relay.
* **A relay that is unreachable is not a relay that is unconfigured.** Nothing
  probes the network at boot: the capability, the registration path and the test
  button all stay live, ordinary sends fail inside the 45-second bound and are
  dropped, and a test reports the real failure. Recovery needs no restart. The
  alternative — selecting the stub because one request failed — would suppress
  every registration until somebody noticed.
* **Relay mode needs a phone that has enrolled.** The daemon stores a relay
  credential and presents it on every send; the relay mints that credential after
  it verifies the phone's App Attest, and the phone stores the opaque bearer it
  receives and hands it to the daemon over the paired socket. A registration
  carrying no credential is
  refused with a reason rather than filed as a device that will never ring — so a
  phone whose App Attest is unsupported, or which has not yet enrolled, gets no
  relay push, and the direct-key override stays available on that Mac.
* **A certificate from Tailscale depends on the tailnet.** `tailscale cert`
  needs HTTPS Certificates enabled for the tailnet, and issues only for this
  node's MagicDNS name. Without it the daemon serves `ws://` and says so; a
  certificate obtained elsewhere and dropped in `~/.codeconnect/tls/` is the way
  round it, and the only route open to a LAN `ws_bind`.
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
* **The connection caps assume the raised file limit.** [Bounds](#bounds) sizes
  them for the 10,240 descriptors ccd raises itself to. Where the hard limit is
  lower — an administrator's `LimitNOFILE`, a systemd older than 240 (hard
  4,096), a container — the caps are not lowered to match: past the real limit
  `accept` fails for want of a descriptor, and the accept loop backs off.
* **`answer` without a `session_id` is resolved by request id.** A client on
  protocol minor 1 cannot say which run it means; the daemon matches the live
  approvals, then the ledger's most recent entry. Two runs holding the same
  `request_id` open at once is refused with an instruction rather than guessed.
* **A daemon that dies just after launching a Codex run's app-server can orphan it.**
  The app-server's identity is recorded right after the launch, in one database
  write. A daemon that dies in those few milliseconds leaves the run with no recorded
  identity: the restarted daemon fails the run, but has nothing it may signal, and
  Codex's app-server never exits by itself, so it runs until logout or reboot. A
  message's relaunch of a Codex run has the same few milliseconds.
* **A daemon that dies while a message relaunches a Codex run fails the run.** The
  restarted daemon fails every Codex run still `starting`, the relaunched one among them,
  and a failed run is not relaunched again: the person starts a new run, as before this
  relaunch existed.
* **Whether a trusted folder's allow rules skip the approval card is not
  measured.** In a Claude Code run that loads its folder's project, that
  project's hooks run; whether its `permissions.allow` rules also let a tool run
  without an approval card has not been measured.
