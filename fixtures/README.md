# fixtures

Real payloads captured from live `claude` 2.1.220 sessions.
They are recordings, not hand-written mocks: the replay tests in
`mac/ccd/src/fixture_replay.rs` run them through the production ingest path, so
a schema drift in a future Claude Code release fails the test suite instead of
failing silently in production.

| Path | What it is |
|---|---|
| `hooks/acceptance-run.jsonl` | Every hook payload from one end-to-end `codeconnect claude` session that went through `cc-hook` -> `ccd` (SessionStart, PreToolUse, PermissionRequest, Notification, PostToolUse, Stop). Daemon-added fields stripped. |
| `hooks/live-hook-payloads.jsonl` | Deduplicated payloads from the hook-semantics probes, including the `permission_prompt` Notification and multiple PermissionRequest shapes. |
| `transcript/session-sample.jsonl` | A real session JSONL slice covering `mode`, `permission-mode`, `file-history-snapshot`, `user`, `attachment`, `ai-title`, `assistant` and `system` entries. |

Two properties are worth stating explicitly, because both were measured rather
than assumed and both drive the design:

* `PermissionRequest` payloads carry **no `tool_use_id`**. The daemon recovers
  one by correlating with the `PreToolUse` that fires immediately before.
* `Notification` carries `notification_type`; `permission_prompt` is the push
  trigger and needs no inference from terminal output.
