# Codex sessions

A Codex session is started from the CodeConnect app: choose this computer, the project and
Codex. The daemon runs Codex's own app server for the session, through your login shell.

## Quota

A Codex session on Codex's **Default** account runs as **you**. It uses your own `CODEX_HOME`
— `~/.codex` unless you set that variable — which is where your Codex sign-in lives. A session
on another Codex account you added in CodeConnect uses that account's own sign-in instead, with
the rest of your Codex home (its settings, trust, skills and sessions) shared. Choosing another
account stops nothing, and new sessions use it at once. An open session moves to it when you next
send it a message, if nothing is running in it: its app server is ended, and your message starts it
again on the same thread with the new sign-in. If something is running (a turn, a command or subagent it
started in its last turn, a message of yours it hasn't answered), that message goes on the old account and
the session moves at the next one. Its menus and settings stay as they are throughout. A message
queued in Codex itself (`codex queue`) is not kept across a move, and a Codex background process
started in an earlier turn, still alive at the first send after a switch, is ended by the move. When the new account's
plan doesn't offer the model the session was on, it carries on with that plan's default model, one
line in the conversation says so, and its model menu offers that plan's models.

For each Codex account, and the Default, the Accounts page shows the email and plan it is signed in
as and its usage as Codex reports it, read from Codex when you open the page and again when a
sign-in finishes (at most once a minute per account otherwise), its usage also as a running
session's Codex says it has changed, and kept in memory only, never written to disk or to the log.
The page answers at once with what is already known, and fills in the rest as it arrives.

So: **turns your phone starts are your turns.** Saying something from the phone spends the
same account allowance as a turn you start in Codex yourself. There is no CodeConnect
account, no proxy key and no separate meter.

## CodeConnect Messages

Whether a session acts on another session's message is one switch per computer, on unless
you set it off: `codeconnect msg-act off` or `on` (an `off` set before is kept). It is kept in
CodeConnect's own folder; nothing is written to the sessions' own files.

## See also

* [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — how the daemon and the phone fit together
* [`mac/README.md`](../mac/README.md) — the Mac side in detail: pairing, TLS, the LaunchAgent
* [`ios/README.md`](../ios/README.md#codex-sessions) — the phone side: the card, the two controls and the one rule that gates them
* [`fixtures/README.md`](../fixtures/README.md) — every recorded Codex measurement and what it proves
