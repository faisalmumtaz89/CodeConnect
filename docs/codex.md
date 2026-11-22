# Codex sessions

A Codex session is started from the CodeConnect app: choose this computer, the project and
Codex. The daemon runs Codex's own app server for the session, through your login shell.

## Quota

A Codex session on Codex's **Default** account runs as **you**. It uses your own `CODEX_HOME`
— `~/.codex` unless you set that variable — which is where your Codex sign-in lives. A session
on another Codex account you added in CodeConnect uses that account's own sign-in, with the rest of your Codex home (its settings, trust, skills and sessions) shared
instead.

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
