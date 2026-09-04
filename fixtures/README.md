# fixtures

Real payloads captured from live `claude` sessions.
They are recordings, not hand-written mocks: the replay tests in
`mac/ccd/src/fixture_replay.rs` run them through the production ingest path, so
a schema drift in a future Claude Code release fails the test suite instead of
failing silently in production.

| Path | What it is |
|---|---|
| `hooks/acceptance-run.jsonl` | Every hook payload from one end-to-end `codeconnect claude` session that went through `cc-hook` -> `ccd` (SessionStart, PreToolUse, PermissionRequest, Notification, PostToolUse, Stop). Daemon-added fields stripped. |
| `hooks/live-hook-payloads.jsonl` | Deduplicated payloads from the hook-semantics probes, including the `permission_prompt` Notification and multiple PermissionRequest shapes. |
| `appattest/attestation-object.b64` | Apple's own published App Attest attestation object, base64 at 76 columns, 5906 bytes decoded. From the DocC payload behind <https://developer.apple.com/documentation/devicecheck/attestation-object-validation-guide> — the rendered page is a script shell, the JSON carries the object. It is the only genuine Apple chain in existence to test against, and the only thing that catches Apple's P-384-key/SHA-256-signature pairing: a chain minted at test time cannot reproduce it, so a narrowed algorithm list would pass every synthetic test and reject every real device. Its credential certificate was valid for three days in April 2026 and has expired, which is why `mac/push-relay/src/attest.rs` takes the verification time as a parameter and its tests pin a clock inside that window rather than reading the system one. |
| `transcript/session-sample.jsonl` | A real session JSONL slice covering `mode`, `permission-mode`, `file-history-snapshot`, `user`, `attachment`, `ai-title`, `assistant` and `system` entries. |
| `panes/composer/` (10) | Screens with a composer that is taking keys: every permission mode, mid-turn, a typed prompt, prompt text wrapped over three rows, a transcript full of `❯` echoes, and a pane three columns wide. The presence check must accept every one. |
| `panes/no-composer/` (6) | Screens with no composer to type into: the permission prompt, the model menu, the rewind menu, the trust dialog, `/status`, and a plain shell that printed a `❯`. Every one of them carries a `❯` somewhere, which is what makes them the set worth having. The presence check must refuse every one. |
| `panes/bounds/` (2) | Real composers the presence check refuses, and must go on refusing. `tall-composer-top-rule-offscreen.txt` holds more text than its 12-row pane is tall, so its opening rule has scrolled off and the `❯` row is the pane's first row. `short-pane-top-rule-clipped.txt` is a four-row pane where the opening rule is clipped and only the *closing* rule is left, directly below the marker. See the tests that name them. |
| `codex/composite_ids.json` | Rust-generated composite wire-id vectors (`protocol::composite_id`); the Swift client's opaque-correlation cross-language pin. |
| `codex/lifecycle.jsonl` (14) | One live-observed Codex app-server turn — the real 0.147 notification stream (`thread/started`, `item/started`/`completed` for a userMessage and an agentMessage, the streamed `item/agentMessage/delta`, `thread/tokenUsage/updated`, `turn/completed{completed}`), plus the observation-noise frames the adapter deliberately drops. Each line is one JSON-RPC frame, exactly as a connection delivers it. Captured from `captures/subscribed/`; from a throwaway project, absolute user paths scrubbed to `/work/…`. Drives `codex_adapter.rs`. |
| `codex/command-execution.jsonl` (30) | A turn that runs a shell command — `commandExecution` `item/started`(inProgress)→`item/completed`(exitCode 0), with `reasoning` items and the `item/commandExecution/requestApproval` + `serverRequest/resolved` frames (approvals are Phase 3; the observation adapter drops them). From `captures/approvals-accept/`. |
| `codex/file-change.jsonl` (44) | A `fileChange` item (`changes[].{path,kind,diff}`) started, approval-requested, and completed. From `captures/approvals-fc-readonly/`. |
| `codex/thread-switch.jsonl` (169) | **The `/new` thread switch, measured on four connections at once** (chunk 2e-4c). One live 0.147 session driven through: a turn on thread A, an observer resumed to A, `/new`, a turn on B, that same observer resumed to B, a `/resume` back to A, and an `thread/unsubscribe`. Each line carries `conn` (which connection delivered it) and `dir` (`c2s`/`s2c`), plus `note` markers naming each step, because the whole point of the capture is *who got what*: the switch signal, the frames a subscription does and does not receive, and the ordering of the `thread/unsubscribe` markers. Observation noise (`plugin/list`, `app/list`, skills, account) and the multi-MB frames are excluded; absolute paths scrubbed to `/work/…`. |
| `codex/session-0.153.jsonl` (16) | **The codex 0.153 re-grounding capture** — one live `codeconnect codex` session on 0.153, recorded by the in-broker frame tee (`codex_broker::frame_tee`, a `--features frame-tee` build plus `CC_CODEX_FRAME_TEE=<path>`), which is the first committed capture taken by tooling that lives in this repository. Sanitised to the `{conn,dir,frame}` shape the older captures use — the tee also records each frame VERBATIM as `raw`, plus `run`/`seq`, which a fixture does not need and which cannot be scrubbed the way a parsed view can. Carries the three frames the 0.153 pins are grounded on: the `thread/start` whose `dynamicTools` declares the `codex_tui` bundle, the `turn/start` carrying `serviceTierForTurn`/`toolOutput`/`turnTrigger`/`cyberAccessProgram` all present-and-null, and the `thread/read` on a FOREIGN thread that `Disposition::ReadSessionThread` refuses. `item/tool/call` shows the model invoking `read_thread` and `wait_threads` with a foreign `threadId`. Streaming deltas, token usage and bootstrap reads excluded as observation noise; absolute paths scrubbed to `/work/…`, the account name length-preserved. **Broker refusals are not in it**: a refusal is synthesised locally and never crosses the upstream leg the tee taps, so this file carries the refused REQUESTS and the verdicts live in the run dir's `broker.log`. |
| `codex/thread-start-operator-config-0.153.jsonl` (1) | **The first capture taken against a real operator's `config.toml`**, rather than the empty test `CODEX_HOME` (`auth.json` and nothing else) that every live gate and both captures above build. One `thread/start` from a live `codeconnect codex` on 0.153, same frame tee and same sanitised `{conn,dir,frame}` shape as `session-0.153.jsonl`; the one absolute path (`runtimeWorkspaceRoots`) scrubbed to `/work/…`. It exists because that difference is load-bearing: the TUI sends `serviceTier` **only** when the user's own config sets `service_tier`, so no capture taken against an empty config could ever have carried it, and `thread/start`'s exhaustive allowlist refused the key as unknown — which killed the session outright, because codex 0.153's TUI exits the moment its first `thread/start` is refused. This file is the measurement that widened the allowlist by one preference field (`codex-broker/src/fingerprint.rs`, `check_thread_start_service_tier`), and the union invariant reads it directly, so the rule and its evidence cannot drift apart. Its 24 keys are the 23 of the 0.153 capture plus `serviceTier`; `sandbox` reads `read-only` because CodeConnect now passes the session sandbox to the TUI explicitly. |
| `codex/dynamic-tools-0.153.json` | The `codex_tui` `dynamicTools` bundle verbatim, from that capture — six tools (`list_threads`, `list_archived_threads`, `read_thread`, `wait_threads`, `set_thread_title`, `set_thread_archived`), canonical form 2728 bytes, sha256 `7c962509…`. `codex-broker/src/fingerprint.rs` compares against this file, so the pin and its evidence cannot drift apart. |
| `codex/developer-instructions-0.153.txt` | codex 0.153's `collaborationMode.settings.developer_instructions`, 1288 bytes, sha256 `1042cc64…`. The 0.147 blob is 925 bytes and the **schema for this field is byte-identical between the two releases** — this file is the evidence for the content-drift half of re-grounding, which the launch-time shape gate structurally cannot see. |
| `codex/model-switch.json` | **Eleven real `turn/start` frames across three models** (chunk 2e-4c), plus the six `thread/settings/update` frames the TUI's `/model` flow emits. The evidence behind the `collaborationMode` re-grounding: `mode` and `settings.developer_instructions` byte-identical on all eleven (and identical to `turn-start-request.json`, captured a week earlier on a different sandbox), while `settings.model` and `settings.reasoning_effort` simply carry whatever the picker last set. `codex-broker/src/fingerprint.rs` reads this file directly in its tests, so the rule and its evidence cannot drift apart. |
| `codex/interrupt.jsonl` (36) | Two turns; the second interrupts a `commandExecution` mid-flight — `turn/completed{status:"interrupted", items:[]}` (D14) leaves the exec non-terminal, so the adapter must synthesize its terminal from what it saw live. From `captures/steer-abort-pending/`. |

The pane captures come from `tmux capture-pane -p -J` against live `claude`
2.1.228 and 2.1.232 sessions. Personal identifiers are replaced by
length-preserving placeholders, so the pane geometry is byte-identical to what
was captured — column counts, wrap points and trailing padding all included,
which is the only reason a row-shaped check can be tested against them at all.

Four properties are worth stating explicitly, because all four were measured
rather than assumed and all four drive the design:

* `PermissionRequest` payloads carry **no `tool_use_id`**. The daemon recovers
  one by correlating with the `PreToolUse` that fires immediately before.
* `Notification` carries `notification_type`; `permission_prompt` is the push
  trigger and needs no inference from terminal output.
* A composer's footer hints are **not** a signal that it is there. `? for
  shortcuts` is dropped as soon as the shift+tab mode hint needs the room, and
  `← for agents` becomes `← 1 agent` once a subagent exists. The box is the
  invariant: at 3, 10 and 20 columns the composer keeps the same rule / `❯`
  prompt row / rule structure, so the check that reads it is width-invariant.
* A Codex `thread/started` is a **global broadcast**: every initialized connection
  receives it, subscribed or not, and it is the ONLY frame about a new thread that a
  connection subscribed elsewhere receives. That is what makes it usable as a switch
  signal and what makes ignoring it fatal.
* `thread/resume` **adds** a subscription rather than replacing one. One connection
  resumed thread A, followed a `/new` to B, resumed B on the same socket, and thereafter
  received both threads' streams. Following a switch therefore costs no reconnect — and
  the per-thread filter is what keeps the two apart.
* Both rows of that box start at **column zero**. An indented one is a
  quotation — agents print captured panes into their own output, and Claude
  indents tool results — so the anchor is what separates a composer that is
  there from a composer somebody wrote about.
