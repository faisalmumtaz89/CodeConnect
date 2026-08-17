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
* Both rows of that box start at **column zero**. An indented one is a
  quotation — agents print captured panes into their own output, and Claude
  indents tool results — so the anchor is what separates a composer that is
  there from a composer somebody wrote about.
