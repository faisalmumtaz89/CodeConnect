# Changelog

User-facing changes, newest first. Mac releases are cut per
[`RELEASING.md`](RELEASING.md); the iPhone app ships on its own App Store track.

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
