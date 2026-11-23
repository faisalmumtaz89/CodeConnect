# Codex sessions

A Codex session is started from the CodeConnect app: choose this computer, the project and
Codex. The daemon runs Codex's own app server for the session, through your login shell.

## Quota

A Codex session on Codex's **Default** account runs as **you**. It uses your own `CODEX_HOME`
— `~/.codex` unless you set that variable — which is where your Codex sign-in lives. A session
on another Codex account you added in CodeConnect uses that account's own sign-in instead, with
the rest of your Codex home (its settings, trust, skills and sessions) shared. Choosing another
account moves each running session to it between turns: once a session is idle its app server
is ended, and your next message to it starts it again on the same thread with the new sign-in.
A message queued in Codex itself (`codex queue`) is not kept across that, and a command or
helper an agent left running in the background is stopped when its session moves. A moved
session's settings can be changed once your next message has started it. When the new account's
plan doesn't offer the model the session was on, it carries on with that plan's default model, one
line in the conversation says so, and its model menu offers that plan's models.

For each Codex account, and the Default, the Accounts page shows the email and plan it is signed
in as and its usage as Codex reports it, read from Codex when you open the page and again when a
sign-in finishes (at most once a minute per account otherwise) and kept in memory only, never
written to disk or to the log. The page answers at once with what is already known, and fills in
the rest as it arrives.

So: **turns your phone starts are your turns.** Saying something from the phone spends the
same account allowance as a turn you start in Codex yourself. There is no CodeConnect
account, no proxy key and no separate meter.

## Messages from other agents

Whether an agent acts on another agent's message is one switch per computer, off until you
set it: `codeconnect mail-trust on` or `off`. It is kept in CodeConnect's own folder; nothing
is written to the agents' own files.

## See also

* [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) — how the daemon and the phone fit together
* [`mac/README.md`](../mac/README.md) — the Mac side in detail: pairing, TLS, the LaunchAgent
* [`ios/README.md`](../ios/README.md#codex-sessions) — the phone side: the card, the two controls and the one rule that gates them
* [`fixtures/README.md`](../fixtures/README.md) — every recorded Codex measurement and what it proves
